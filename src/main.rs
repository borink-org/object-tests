use object_tests::{
    Result,
    expected_unsupported::{
        entry_covers, find_verdict_mismatches, load_expected_unsupported_cases,
        record_unsupported_cases, suite_name, unknown_listed_case_ids,
    },
    model::{Case, Lane, Suite},
    runner,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::ExitCode};

const USAGE: &str = concat!(
    "usage: object-tests validate SUITE | ",
    "grade SUITE [OPTIONS] -- ADAPTER [ARGS...] | ",
    "live SUITE CONFIG [OPTIONS] -- ADAPTER [ARGS...]; ",
    "OPTIONS: --case ID, --provider PROVIDER, --profile PROFILE, --jobs N, ",
    "--expected-unsupported FILE or --record-unsupported FILE",
);

struct GradingOptions<'a> {
    suite_path: &'a str,
    live_config_path: Option<&'a str>,
    case_id: Option<&'a str>,
    provider_name: Option<&'a str>,

    /// Grade only the cases of this profile, such as `s3-express`, where one provider
    /// has several.
    profile_name: Option<&'a str>,

    /// The number of cases graded at once, or one per CPU if absent.
    job_count: Option<&'a str>,

    /// Check every verdict against this list of expected unsupported cases.
    expected_unsupported_path: Option<&'a str>,

    /// Rewrite this list with the cases graded unsupported.
    record_unsupported_path: Option<&'a str>,
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
        profile_name: None,
        job_count: None,
        expected_unsupported_path: None,
        record_unsupported_path: None,
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
            "--profile" => &mut grading_options.profile_name,
            "--jobs" => &mut grading_options.job_count,
            "--expected-unsupported" => &mut grading_options.expected_unsupported_path,
            "--record-unsupported" => &mut grading_options.record_unsupported_path,
            _ => return Err(format!("unknown option {option_name}").into()),
        };
        if selected_option.replace(option_value).is_some() {
            return Err(format!("duplicate option {option_name}").into());
        }
    }
    if grading_options.expected_unsupported_path.is_some()
        && grading_options.record_unsupported_path.is_some()
    {
        return Err("--expected-unsupported and --record-unsupported exclude each other".into());
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
        .filter(|case| {
            grading_options
                .profile_name
                .is_none_or(|profile| profile == case.profile)
        })
        .collect();
    if cases.is_empty() {
        return Err("no cases selected".into());
    }

    let suite_section_name = suite_name(grading_options.suite_path)?;
    let expected_unsupported_cases = match grading_options.expected_unsupported_path {
        Some(path) => {
            let mut expected_lists = load_expected_unsupported_cases(path, true)?;
            let listed_cases = expected_lists
                .remove(&suite_section_name)
                .unwrap_or_default();
            let unknown_case_ids = unknown_listed_case_ids(&listed_cases, &suite);
            if !unknown_case_ids.is_empty() {
                return Err(format!(
                    "{path} lists cases {suite_section_name} does not have: {}",
                    unknown_case_ids.join(", ")
                )
                .into());
            }
            Some(listed_cases)
        }
        None => None,
    };
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
    let mut has_wrong_or_failed_cases = false;
    let mut graded_reports = Vec::new();
    let graded_whole_suite = cases.len() == suite.cases.len();
    let worker_count = match grading_options.job_count {
        Some(job_count) => job_count
            .parse::<usize>()
            .ok()
            .filter(|&job_count| job_count > 0)
            .ok_or_else(|| format!("--jobs takes a positive number, got {job_count}"))?,
        None => std::thread::available_parallelism().map_or(1, usize::from),
    };
    let reports = runner::grade_cases(
        &cases,
        &suite.profiles,
        grading_options.adapter_command,
        config.as_ref(),
        worker_count,
    )?;
    for (case, report) in cases.into_iter().zip(reports) {
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
        has_wrong_or_failed_cases |= verdict != "pass" && verdict != "unsupported";
        println!("{report}");
        graded_reports.push(report);
    }

    let mut summary = json!({
        "summary": verdict_counts,
        "authentication": if config.is_some() {
            "live results above; scoped to the configured identity and operations"
        } else {
            "not verified live"
        },
    });
    let mut run_failed = has_nonpassing_cases;

    if let Some(listed_cases) = &expected_unsupported_cases {
        let mismatches = find_verdict_mismatches(listed_cases, &graded_reports, graded_whole_suite);
        run_failed = !mismatches.is_empty();
        summary["expected_unsupported"] = json!({"mismatches": mismatches});
    }

    if let Some(path) = grading_options.record_unsupported_path {
        let mut expected_lists = load_expected_unsupported_cases(path, false)?;
        let listed_cases = expected_lists.entry(suite_section_name).or_default();
        record_unsupported_cases(listed_cases, &graded_reports);
        // Keep the patterns, and drop the entries that cover no case of this suite.
        listed_cases.retain(|entry, _| suite.cases.iter().any(|case| entry_covers(entry, case)));
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&expected_lists)?),
        )?;
        run_failed = has_wrong_or_failed_cases;
    }

    println!("{summary}");
    Ok(if run_failed {
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
            "grade suite --expected-unsupported a --record-unsupported b -- adapter",
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
