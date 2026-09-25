use object_tests::{
    Result,
    model::{Case, Lane, Suite},
    runner,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::ExitCode};

const USAGE: &str = concat!(
    "usage: object-tests validate SUITE | ",
    "grade SUITE [--case ID] [--provider PROVIDER] -- ADAPTER [ARGS...] | ",
    "live SUITE CONFIG [--case ID] [--provider PROVIDER] -- ADAPTER [ARGS...]",
);

struct GradingOptions<'a> {
    suite_path: &'a str,
    live_config_path: Option<&'a str>,
    case_id: Option<&'a str>,
    provider_name: Option<&'a str>,
    adapter_command: &'a [String],
}

enum CommandLineInvocation<'a> {
    Validate(&'a str),
    Run(GradingOptions<'a>),
}

fn parse_command_line_arguments(arguments: &[String]) -> Result<CommandLineInvocation<'_>> {
    let (subcommand, remaining_arguments) = arguments.split_first().ok_or(USAGE)?;
    if subcommand == "validate" {
        return match remaining_arguments {
            [suite_path] => Ok(CommandLineInvocation::Validate(suite_path)),
            _ => Err(USAGE.into()),
        };
    }
    let (suite_path, remaining_arguments) = remaining_arguments.split_first().ok_or(USAGE)?;
    let (live_config_path, remaining_arguments) = match subcommand.as_str() {
        "grade" => (None, remaining_arguments),
        "live" => {
            let (config_path, remaining_arguments) =
                remaining_arguments.split_first().ok_or(USAGE)?;
            (Some(config_path.as_str()), remaining_arguments)
        }
        _ => return Err(USAGE.into()),
    };
    let adapter_separator_index = remaining_arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or(USAGE)?;
    let adapter_command = &remaining_arguments[adapter_separator_index + 1..];
    if adapter_command.is_empty() {
        return Err("missing adapter command after --".into());
    }
    let mut grading_options = GradingOptions {
        suite_path,
        live_config_path,
        case_id: None,
        provider_name: None,
        adapter_command,
    };
    let mut grader_arguments = remaining_arguments[..adapter_separator_index].iter();
    while let Some(option_name) = grader_arguments.next() {
        let option_value = grader_arguments.next().ok_or(USAGE)?;
        if option_value.starts_with("--") {
            return Err(format!("missing value for {option_name}").into());
        }
        let selected_option = match option_name.as_str() {
            "--case" => &mut grading_options.case_id,
            "--provider" => &mut grading_options.provider_name,
            _ => return Err(format!("unknown option {option_name}").into()),
        };
        if selected_option.replace(option_value).is_some() {
            return Err(format!("duplicate option {option_name}").into());
        }
    }
    Ok(CommandLineInvocation::Run(grading_options))
}

fn grade_selected_cases(grading_options: GradingOptions<'_>) -> Result<ExitCode> {
    let suite = Suite::load(grading_options.suite_path)?;
    let config: Option<BTreeMap<String, Value>> = match grading_options.live_config_path {
        Some(path) => Some(serde_json::from_slice(&std::fs::read(path)?)?),
        None => None,
    };
    let cases: Vec<&Case> = suite
        .cases
        .iter()
        .filter(|case| matches!(case.lane, Lane::Live) == config.is_some())
        .filter(|case| grading_options.case_id.is_none_or(|id| id == case.id))
        .filter(|case| {
            grading_options
                .provider_name
                .is_none_or(|provider| provider == suite.profiles[&case.profile].provider)
        })
        .collect();
    if cases.is_empty() {
        return Err("no cases selected".into());
    }
    // Resolve every selected endpoint before starting any adapter.
    if let Some(config) = &config {
        for case in &cases {
            let endpoint = config
                .get(&case.profile)
                .ok_or_else(|| format!("missing live endpoint for {}", case.profile))?;
            if !endpoint["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://"))
            {
                return Err("live endpoint must use HTTPS".into());
            }
        }
    }
    let mut verdict_counts = BTreeMap::<&str, BTreeMap<String, usize>>::new();
    let mut has_nonpassing_cases = false;
    for case in cases {
        let endpoint = config.as_ref().map(|config| &config[&case.profile]);
        let report = runner::grade_case(
            case,
            &suite.profiles[&case.profile],
            grading_options.adapter_command,
            endpoint,
        )?;
        let verdict = report["verdict"]
            .as_str()
            .expect("runner returns a verdict");
        let lane = match case.lane {
            Lane::Core => "core",
            Lane::Vectors => "vectors",
            Lane::Live => "live",
        };
        *verdict_counts
            .entry(lane)
            .or_default()
            .entry(verdict.to_owned())
            .or_default() += 1;
        has_nonpassing_cases |= verdict != "pass";
        println!("{report}");
    }
    println!(
        "{}",
        json!({
            "summary": verdict_counts,
            "authentication": if config.is_some() {
                "live results above; scoped to the configured identity and operations"
            } else {
                "not verified live"
            },
        })
    );
    Ok(if has_nonpassing_cases {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn execute_command_line() -> Result<ExitCode> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    match parse_command_line_arguments(&arguments)? {
        CommandLineInvocation::Validate(path) => {
            let suite = Suite::load(path)?;
            println!("{} cases validated", suite.cases.len());
            Ok(ExitCode::SUCCESS)
        }
        CommandLineInvocation::Run(grading_options) => grade_selected_cases(grading_options),
    }
}

fn main() -> ExitCode {
    match execute_command_line() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_unknown_and_repeated_arguments() {
        for arguments in [
            "",
            "--",
            "grade --",
            "validate suite extra",
            "grade suite --",
            "grade suite --case -- adapter",
            "grade suite --wat value -- adapter",
            "grade suite --case a --case b -- adapter",
            "live suite -- adapter",
        ] {
            let arguments: Vec<_> = arguments.split_whitespace().map(str::to_owned).collect();
            assert!(
                parse_command_line_arguments(&arguments).is_err(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn adapter_arguments_are_not_grader_options() {
        let arguments: Vec<_> = "grade suite --provider azure -- adapter --case own-argument"
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let CommandLineInvocation::Run(grading_options) =
            parse_command_line_arguments(&arguments).unwrap()
        else {
            panic!()
        };
        assert_eq!(grading_options.provider_name, Some("azure"));
        assert_eq!(
            grading_options.adapter_command,
            ["adapter", "--case", "own-argument"]
        );
    }
}
