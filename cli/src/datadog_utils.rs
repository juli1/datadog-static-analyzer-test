use std::env;

use anyhow::{anyhow, Context, Result};
use kernel::model::rule::Rule;
use kernel::model::ruleset::RuleSet;

use crate::model::datadog_api::ApiResponse;

const DEFAULT_DATADOG_SITE: &str = "datadoghq.com";

// Get all the rules from different rulesets from Datadog
pub fn get_rules_from_rulesets(rulesets_name: &[String]) -> Result<Vec<Rule>> {
    let mut rules: Vec<Rule> = Vec::new();
    for ruleset_name in rulesets_name {
        rules.extend(get_ruleset(ruleset_name)?.rules);
    }
    Ok(rules)
}

// Get environment variables for Datadog. First try to get the variables
// prefixed with DD_ and then, try DATADOG_.
// If nothing works, just returns an error.
pub fn get_datadog_variable_value(variable: &str) -> anyhow::Result<String> {
    let prefixes = vec!["DD", "DATADOG"];
    for prefix in prefixes {
        let name = format!("{}_{}", prefix, variable);
        let var_content = env::var(name);
        if let Ok(var_value) = var_content {
            return Ok(var_value);
        }
    }
    Err(anyhow!("cannot find variable value"))
}

// get rules from one ruleset at datadog
// it connects to the API using the DD_SITE, DD_APP_KEY and DD_API_KEY and retrieve
// the rulesets. We then extract all the rulesets
pub fn get_ruleset(ruleset_name: &str) -> Result<RuleSet> {
    let site = get_datadog_variable_value("SITE").unwrap_or(DEFAULT_DATADOG_SITE.to_string());
    let app_key = get_datadog_variable_value("APP_KEY");
    let api_key = get_datadog_variable_value("API_KEY");
    let url = format!(
        "https://api.{}/api/v2/static-analysis/rulesets/{}",
        site, ruleset_name
    );

    let request_builder = reqwest::blocking::Client::new()
        .get(url)
        .header("Content-Type", "application/json");

    // only add datadog credentials if both app-key and api-keys are defined.
    let request_builder_with_auth = match (app_key, api_key) {
        (Ok(appk), Ok(apik)) => request_builder
            .header("dd-api-key", apik)
            .header("dd-application-key", appk),
        _ => request_builder,
    };

    Ok(request_builder_with_auth
        .send()
        .context("error querying rulesets")?
        .json::<ApiResponse>()
        .context("error when parsing the server response")?
        .into_ruleset())
}
