use biome_analyze::{
    AnalysisFilter, AnalyzerAction, AnalyzerPluginSlice, ControlFlow, Never, RuleFilter,
};
use biome_css_parser::{CssParserOptions, parse_css};
use biome_css_syntax::{CssFileSource, CssLanguage};
use biome_diagnostics::advice::CodeSuggestionAdvice;
use biome_fs::OsFileSystem;
use biome_plugin_loader::AnalyzerGritPlugin;
use biome_rowan::AstNode;
use biome_test_utils::{
    CheckActionType, assert_diagnostics_expectation_comment, assert_errors_are_absent,
    code_fix_to_string, create_analyzer_options, diagnostic_to_string,
    has_bogus_nodes_or_empty_slots, parse_test_path, register_leak_checker, scripts_from_json,
    write_analyzer_snapshot,
};
use camino::Utf8Path;
use std::ops::Deref;
use std::sync::Arc;
use std::{fs::read_to_string, slice};

tests_macros::gen_tests! {"tests/specs/**/*.{css,json,jsonc}", crate::run_test, "module"}
tests_macros::gen_tests! {"tests/suppression/**/*.{css,json,jsonc}", crate::run_suppression_test, "module"}
tests_macros::gen_tests! {"tests/plugin/*.grit", crate::run_plugin_test, "module"}

fn run_test(input: &'static str, _: &str, _: &str, _: &str) {
    register_leak_checker();

    let input_file = Utf8Path::new(input);
    let file_name = input_file.file_name().unwrap();

    let (group, rule) = parse_test_path(input_file);
    if rule == "specs" || rule == "suppression" {
        panic!("the test file must be placed in the {rule}/<group-name>/<rule-name>/ directory");
    }
    if group == "specs" || group == "suppression" {
        panic!("the test file must be placed in the {group}/{rule}/<rule-name>/ directory");
    }
    if biome_css_analyze::METADATA
        .deref()
        .find_rule(group, rule)
        .is_none()
    {
        panic!("could not find rule {group}/{rule}");
    }

    let rule_filter = RuleFilter::Rule(group, rule);
    let filter = AnalysisFilter {
        enabled_rules: Some(slice::from_ref(&rule_filter)),
        ..AnalysisFilter::default()
    };

    let mut snapshot = String::new();
    let extension = input_file.extension().unwrap_or_default();

    let parser_options = if file_name.ends_with(".module.css") {
        CssParserOptions {
            css_modules: true,
            ..CssParserOptions::default()
        }
    } else {
        CssParserOptions::default()
    };

    let input_code = read_to_string(input_file)
        .unwrap_or_else(|err| panic!("failed to read {input_file:?}: {err:?}"));

    if let Some(scripts) = scripts_from_json(extension, &input_code) {
        for script in scripts {
            analyze_and_snap(
                &mut snapshot,
                &script,
                CssFileSource::css(),
                filter,
                file_name,
                input_file,
                CheckActionType::Lint,
                parser_options,
                &[],
            );
        }
    } else {
        let Ok(source_type) = input_file.try_into() else {
            return;
        };
        analyze_and_snap(
            &mut snapshot,
            &input_code,
            source_type,
            filter,
            file_name,
            input_file,
            CheckActionType::Lint,
            parser_options,
            &[],
        );
    };

    insta::with_settings!({
        prepend_module_to_snapshot => false,
        snapshot_path => input_file.parent().unwrap(),
    }, {
        insta::assert_snapshot!(file_name, snapshot, file_name);
    });
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn analyze_and_snap(
    snapshot: &mut String,
    input_code: &str,
    source_type: CssFileSource,
    filter: AnalysisFilter,
    file_name: &str,
    input_file: &Utf8Path,
    check_action_type: CheckActionType,
    parser_options: CssParserOptions,
    plugins: AnalyzerPluginSlice,
) {
    let parsed = parse_css(input_code, parser_options);
    let root = parsed.tree();

    let mut diagnostics = Vec::new();
    let mut code_fixes = Vec::new();
    let options = create_analyzer_options(input_file, &mut diagnostics);

    let (_, errors) = biome_css_analyze::analyze(&root, filter, &options, plugins, |event| {
        if let Some(mut diag) = event.diagnostic() {
            for action in event.actions() {
                if check_action_type.is_suppression() {
                    if action.is_suppression() {
                        check_code_action(
                            input_file,
                            input_code,
                            source_type,
                            &action,
                            parser_options,
                        );
                        diag = diag.add_code_suggestion(CodeSuggestionAdvice::from(action));
                    }
                } else if !action.is_suppression() {
                    check_code_action(input_file, input_code, source_type, &action, parser_options);
                    diag = diag.add_code_suggestion(CodeSuggestionAdvice::from(action));
                }
            }

            diagnostics.push(diagnostic_to_string(file_name, input_code, diag.into()));
            return ControlFlow::Continue(());
        }

        for action in event.actions() {
            if check_action_type.is_suppression() {
                if action.category.matches("quickfix.suppressRule") {
                    check_code_action(input_file, input_code, source_type, &action, parser_options);
                    code_fixes.push(code_fix_to_string(input_code, action));
                }
            } else if !action.category.matches("quickfix.suppressRule") {
                check_code_action(input_file, input_code, source_type, &action, parser_options);
                code_fixes.push(code_fix_to_string(input_code, action));
            }
        }

        ControlFlow::<Never>::Continue(())
    });

    for error in errors {
        diagnostics.push(diagnostic_to_string(file_name, input_code, error));
    }

    write_analyzer_snapshot(
        snapshot,
        input_code,
        diagnostics.as_slice(),
        code_fixes.as_slice(),
        "css",
    );

    assert_diagnostics_expectation_comment(input_file, root.syntax(), diagnostics.len());
}

fn check_code_action(
    path: &Utf8Path,
    source: &str,
    _source_type: CssFileSource,
    action: &AnalyzerAction<CssLanguage>,
    options: CssParserOptions,
) {
    let (new_tree, text_edit) = match action
        .mutation
        .clone()
        .commit_with_text_range_and_edit(true)
    {
        (new_tree, Some((_, text_edit))) => (new_tree, text_edit),
        (new_tree, None) => (new_tree, Default::default()),
    };

    let output = text_edit.new_string(source);

    // Checks that applying the text edits returned by the BatchMutation
    // returns the same code as printing the modified syntax tree
    assert_eq!(new_tree.to_string(), output);

    if has_bogus_nodes_or_empty_slots(&new_tree) {
        panic!("modified tree has bogus nodes or empty slots:\n{new_tree:#?} \n\n {new_tree}")
    }

    // Checks the returned tree contains no missing children node
    if format!("{new_tree:?}").contains("missing (required)") {
        panic!("modified tree has missing children:\n{new_tree:#?}")
    }

    // Re-parse the modified code and panic if the resulting tree has syntax errors
    let re_parse = parse_css(&output, options);
    assert_errors_are_absent(re_parse.tree().syntax(), re_parse.diagnostics(), path);
}

pub(crate) fn run_suppression_test(input: &'static str, _: &str, _: &str, _: &str) {
    register_leak_checker();

    let input_file = Utf8Path::new(input);
    let file_name = input_file.file_name().unwrap();
    let input_code = read_to_string(input_file)
        .unwrap_or_else(|err| panic!("failed to read {input_file:?}: {err:?}"));

    let (group, rule) = parse_test_path(input_file);

    let rule_filter = RuleFilter::Rule(group, rule);
    let filter = AnalysisFilter {
        enabled_rules: Some(slice::from_ref(&rule_filter)),
        ..AnalysisFilter::default()
    };

    let mut snapshot = String::new();
    analyze_and_snap(
        &mut snapshot,
        &input_code,
        CssFileSource::css(),
        filter,
        file_name,
        input_file,
        CheckActionType::Suppression,
        CssParserOptions::default(),
        &[],
    );

    insta::with_settings!({
        prepend_module_to_snapshot => false,
        snapshot_path => input_file.parent().unwrap(),
    }, {
        insta::assert_snapshot!(file_name, snapshot, file_name);
    });
}

fn run_plugin_test(input: &'static str, _: &str, _: &str, _: &str) {
    register_leak_checker();

    let plugin_path = Utf8Path::new(input);
    let file_name = plugin_path.file_name().unwrap();
    let input_path = plugin_path.with_extension("css");

    let plugin = match AnalyzerGritPlugin::load(
        &OsFileSystem::new(plugin_path.to_owned()),
        Utf8Path::new(plugin_path),
    ) {
        Ok(plugin) => plugin,
        Err(err) => panic!("Cannot load plugin: {err:?}"),
    };

    let filter = AnalysisFilter {
        enabled_rules: Some(&[]),
        ..AnalysisFilter::default()
    };

    let mut snapshot = String::new();

    let input_code = read_to_string(&input_path)
        .unwrap_or_else(|err| panic!("failed to read {input_path:?}: {err:?}"));
    let Ok(source_type) = input_path.as_path().try_into() else {
        return;
    };
    analyze_and_snap(
        &mut snapshot,
        &input_code,
        source_type,
        filter,
        file_name,
        &input_path,
        CheckActionType::Lint,
        CssParserOptions::default(),
        &[Arc::new(Box::new(plugin))],
    );

    insta::with_settings!({
        prepend_module_to_snapshot => false,
        snapshot_path => plugin_path.parent().unwrap(),
    }, {
        insta::assert_snapshot!(file_name, snapshot, file_name);
    });
}
