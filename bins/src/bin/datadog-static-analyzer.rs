use cli::config_file::read_config_file;
use cli::datadog_utils::get_rules_from_rulesets;
use cli::file_utils::{filter_files_for_language, get_files};
use cli::model::config_file::ConfigFile;
use cli::rule_utils::{get_languages_for_rules, get_rulesets_from_file};
use itertools::Itertools;
use kernel::analysis::analyze::analyze;
use kernel::constants::VERSION;
use kernel::model::analysis::{AnalysisOptions, ERROR_RULE_TIMEOUT};
use kernel::model::common::OutputFormat;
use kernel::model::rule::{Rule, RuleInternal, RuleResult};

use anyhow::{Context, Result};
use cli::model::cli_configuration::CliConfiguration;
use cli::sarif::sarif_utils::generate_sarif_report;
use getopts::Options;
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::prelude::*;
use std::process::exit;
use std::time::SystemTime;
use std::{env, fs};

fn print_usage(program: &str, opts: Options) {
    let brief = format!("Usage: {} FILE [options]", program);
    print!("{}", opts.usage(&brief));
}

fn print_configuration(configuration: &CliConfiguration) {
    let configuration_method = if configuration.use_configuration_file {
        "config file (static-analysis.datadog.[yml|yaml])"
    } else {
        "rule file"
    };

    let output_format_str = match configuration.output_format {
        OutputFormat::Sarif => "sarif",
        OutputFormat::Json => "json",
    };

    let languages = get_languages_for_rules(&configuration.rules);
    let languages_string: Vec<String> = languages.iter().map(|l| l.to_string()).collect();

    println!("Configuration");
    println!("=============");
    println!("config method    : {}", configuration_method);
    println!("cores available  : {}", num_cpus::get());
    println!("cores used       : {}", configuration.num_cpus);
    println!("#rules loaded    : {}", configuration.rules.len());
    println!("source directory : {}", configuration.source_directory);
    println!("output file      : {}", configuration.output_file);
    println!("output format    : {}", output_format_str);
    println!(
        "ignore paths     : {}",
        configuration.ignore_paths.join(",")
    );
    println!(
        "use config file  : {}",
        configuration.use_configuration_file
    );
    println!("use debug        : {}", configuration.use_debug);
    println!("rules languages  : {}", languages_string.join(","));
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();
    let mut opts = Options::new();
    #[allow(unused_assignments)]
    let mut use_configuration_file = false;
    opts.optopt(
        "i",
        "directory",
        "directory to scan (valid existing directory)",
        "/path/to/code/to/analyze",
    );
    opts.optopt(
        "r",
        "rules",
        "rules to use (json file)",
        "/path/to/rules.json",
    );
    opts.optopt("d", "debug", "use debug mode", "yes/no");
    opts.optopt("f", "format", "format", "json/sarif");
    opts.optopt("o", "output", "output file", "output.json");
    opts.optopt("c", "cpus", "set the number of CPU, use to parallelize (default is the number of cores on the platform)", "--cpus 5");
    opts.optmulti("p", "ignore-path", "path to ignore", "**/test*.py");
    opts.optflag("h", "help", "print this help");
    opts.optflag("v", "version", "shows the version");
    opts.optflag(
        "x",
        "performance-statistics",
        "enable performance statistics",
    );

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            panic!("error when parsing arguments: {}", f)
        }
    };

    if matches.opt_present("v") {
        println!("{}", VERSION);
        exit(1);
    }

    if matches.opt_present("h") {
        print_usage(&program, opts);
        exit(1);
    }

    if !matches.opt_present("o") {
        eprintln!("output file not specified");
        print_usage(&program, opts);
        exit(1);
    }

    let enable_performance_statistics = matches.opt_present("x");

    let output_format = match matches.opt_str("f") {
        Some(f) => {
            if f == "sarif" {
                OutputFormat::Sarif
            } else {
                OutputFormat::Json
            }
        }
        None => OutputFormat::Json,
    };

    let use_debug = *matches
        .opt_str("d")
        .map(|value| value == "yes")
        .get_or_insert(false);
    let output_file = matches
        .opt_str("o")
        .context("output file must be specified")?;

    // Show the ignore paths
    let mut ignore_paths: Vec<String> = Vec::new();
    let ignore_paths_from_options = matches.opt_strs("p");
    let directory_to_analyze_option = matches.opt_str("i");

    let rules_file = matches.opt_str("r");

    if directory_to_analyze_option.is_none() {
        eprintln!("no directory passed, specify a directory with option -i");
        print_usage(&program, opts);
        exit(1)
    }

    let directory_to_analyze = directory_to_analyze_option.unwrap();
    let directory_path = std::path::Path::new(&directory_to_analyze);

    if !directory_path.is_dir() {
        eprintln!("directory to analyze is not correct");
        exit(1)
    }

    let configuration_file: Option<ConfigFile> =
        read_config_file(directory_to_analyze.as_str()).unwrap();
    let mut rules: Vec<Rule> = Vec::new();

    // if there is a configuration file, we load the rules from it. But it means
    // we cannot have the rule parameter given.
    if let Some(conf) = configuration_file {
        use_configuration_file = true;
        if rules_file.is_some() {
            eprintln!("a rule file cannot be specified when a configuration file is present.");
            exit(1);
        }

        let rules_from_api = get_rules_from_rulesets(&conf.rulesets);
        rules.extend(rules_from_api.context("error when reading rules from API")?);

        // copy the ignore paths from the configuration file
        if let Some(v) = conf.ignore_paths {
            ignore_paths.extend(v);
        }
    } else {
        use_configuration_file = false;
        // if there is no config file, we must read the rules from a file.
        // Otherwise, we exit.
        if rules_file.is_none() {
            eprintln!("no configuration and no rule files specified. Please have a static-analysis.datadog.yml file or specify rules with -r");
            print_usage(&program, opts);
            exit(1);
        }

        let rulesets_from_file = get_rulesets_from_file(rules_file.clone().unwrap().as_str());
        let rules_from_file: Vec<Rule> = rulesets_from_file
            .context("cannot read ruleset")?
            .iter()
            .flat_map(|v| v.rules.clone())
            .collect();
        rules.extend(rules_from_file);
    }

    ignore_paths.extend(ignore_paths_from_options);

    let languages = get_languages_for_rules(&rules);

    let files_to_analyze = get_files(directory_to_analyze.as_str(), &ignore_paths)
        .expect("unable to get the list of files to analyze");

    // we try to get the amount of cpu from the system. if the user set an option to force
    // the value. it overrides the value.
    let num_cpus = matches
        .opt_str("c")
        .map(|x| x.parse::<usize>().unwrap())
        .unwrap_or(num_cpus::get());

    // build the configuration object that contains how the CLI should behave.
    let configuration = CliConfiguration {
        use_debug,
        use_configuration_file,
        source_directory: directory_to_analyze.clone(),
        ignore_paths,
        rules_file,
        output_format,
        num_cpus,
        rules,
        output_file,
    };

    print_configuration(&configuration);

    let mut all_rule_results = vec![];

    let analysis_options = AnalysisOptions {
        log_output: true,
        use_debug,
    };

    // we always keep one thread free and some room for the management threads that monitor
    // the rule execution.
    let ideal_threads = ((configuration.num_cpus as f32 - 1.0) * 0.90) as usize;
    let num_threads = if ideal_threads == 0 { 1 } else { ideal_threads };

    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build_global()?;

    let mut total_files_analyzed: usize = 0;
    let start_timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    for language in &languages {
        let files_for_language = filter_files_for_language(&files_to_analyze, language);

        total_files_analyzed += files_for_language.len();

        let rules_for_language: Vec<RuleInternal> = configuration
            .rules
            .iter()
            .filter(|r| r.language == *language)
            .map(|r| {
                r.to_rule_internal()
                    .context("cannot convert to rule internal")
            })
            .collect::<Result<Vec<_>>>()?;

        if use_debug {
            println!(
                "Analyzing {}, {} files detected",
                language,
                files_for_language.len()
            )
        }

        // take the relative path for the analysis
        all_rule_results = files_for_language
            .into_par_iter()
            .flat_map(|path| match fs::read_to_string(&path) {
                Ok(file_content) => analyze(
                    language,
                    rules_for_language.clone(),
                    path.strip_prefix(directory_path)
                        .unwrap()
                        .to_str()
                        .expect("path contains non-Unicode characters"),
                    &file_content,
                    &analysis_options,
                ),
                Err(_) => {
                    eprintln!("error when getting content of path {}", &path.display());
                    vec![]
                }
            })
            .collect();
    }

    let end_timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let nb_violations: u32 = all_rule_results
        .iter()
        .map(|x| x.violations.len() as u32)
        .sum();

    println!(
        "Found {} violations in {} files using {} rules within {} secs",
        nb_violations,
        total_files_analyzed,
        configuration.rules.len(),
        end_timestamp - start_timestamp
    );

    // If the performance statistics are enabled, we show the total execution time per rule
    // and the rule that timed-out.
    if enable_performance_statistics {
        let mut rules_execution_time_ms: HashMap<String, u128> = HashMap::new();

        // first, get the rule execution time
        for rule_result in &all_rule_results {
            let current_value = rules_execution_time_ms
                .get(&rule_result.rule_name)
                .unwrap_or(&0u128);
            let new_value = current_value + rule_result.execution_time_ms;
            rules_execution_time_ms.insert(rule_result.rule_name.clone(), new_value);
        }

        println!("Rule execution time");
        println!("-------------------");
        // Show execution time, in sorted order
        for v in rules_execution_time_ms
            .iter()
            .sorted_by(|a, b| Ord::cmp(b.1, a.1))
            .as_slice()
        {
            println!("rule {:?} execution time {:?} ms", v.0, v.1);
        }

        // show the rules that timed out
        println!("Rule timed out");
        println!("--------------");
        let rules_timed_out: Vec<RuleResult> = all_rule_results
            .clone()
            .into_iter()
            .filter(|r| r.errors.contains(&ERROR_RULE_TIMEOUT.to_string()))
            .collect();
        if rules_timed_out.is_empty() {
            println!("No rule timed out");
        }
        for v in rules_timed_out {
            println!("Rule {} timed out on file {}", v.rule_name, v.filename);
        }
    }

    let value = match configuration.output_format {
        OutputFormat::Json => serde_json::to_string(&all_rule_results),
        OutputFormat::Sarif => match generate_sarif_report(&configuration.rules, &all_rule_results)
        {
            Ok(report) => serde_json::to_string(&report),
            Err(_) => {
                panic!("Error when generating the sarif report");
            }
        },
    };

    // write the reports
    let mut file = fs::File::create(configuration.output_file).context("cannot create file")?;
    file.write_all(value.expect("cannot get data").as_bytes())
        .context("error when writing results")?;
    Ok(())
}
