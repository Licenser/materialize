// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

//! Integration test driver for Materialize.

#![warn(missing_docs)]

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use action::Run;
use anyhow::{anyhow, Context};

use mz_ore::error::ErrorExt;

use self::action::ControlFlow;
use self::error::{ErrorLocation, PosError};
use self::parser::LineReader;
use self::parser::{BuiltinCommand, Command};

mod action;
mod error;
mod format;
mod parser;
mod util;

pub use self::action::Config;
pub use self::error::Error;

/// Runs a testdrive script stored in a file.
pub async fn run_file(config: &Config, filename: &Path) -> Result<(), Error> {
    let mut file =
        File::open(filename).with_context(|| format!("opening {}", filename.display()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("reading {}", filename.display()))?;
    run_string(config, Some(filename), &contents).await
}

/// Runs a testdrive script from the standard input.
pub async fn run_stdin(config: &Config) -> Result<(), Error> {
    let mut contents = String::new();
    io::stdin()
        .read_to_string(&mut contents)
        .context("reading <stdin>")?;
    run_string(config, None, &contents).await
}

/// Runs a testdrive script stored in a string.
///
/// The script in `contents` is used verbatim. The provided `filename` is used
/// only as output in error messages and such. No attempt is made to read
/// `filename`.
pub async fn run_string(
    config: &Config,
    filename: Option<&Path>,
    contents: &str,
) -> Result<(), Error> {
    if let Some(f) = filename {
        println!("--- {}", f.display());
    }

    let mut line_reader = LineReader::new(contents);
    run_line_reader(config, &mut line_reader)
        .await
        .map_err(|e| {
            let location = e.pos.map(|pos| {
                let (line, col) = line_reader.line_col(pos);
                ErrorLocation::new(filename, contents, line, col)
            });
            Error::new(e.source, location)
        })
}

pub(crate) async fn run_line_reader(
    config: &Config,
    line_reader: &mut LineReader<'_>,
) -> Result<(), PosError> {
    // TODO(benesch): consider sharing state between files, to avoid
    // reconnections for every file. For now it's nice to not open any
    // connections until after parsing.
    let cmds = parser::parse(line_reader)?;

    let has_kafka_cmd = cmds.iter().any(|cmd| {
        matches!(
            &cmd.command,
            Command::Builtin(BuiltinCommand { name, .. }) if name.starts_with("kafka-"),
        )
    });

    let (mut state, state_cleanup) = action::create_state(config).await?;

    if config.reset {
        // Delete any existing Materialize and Kafka state *before* the test
        // script starts. We don't clean up Materialize or Kafka state at the
        // end of the script because it's useful to leave the state around,
        // e.g., for debugging, or when using a testdrive script to set up
        // Materialize for further tinkering.

        state.reset_materialize().await?;

        // Only try to clean up Kafka state if the test script uses a Kafka
        // action. Tests that don't use Kafka likely don't have a Kafka
        // broker available.
        if has_kafka_cmd {
            state.reset_kafka().await?;
        }
    }

    let mut errors = Vec::new();

    for cmd in cmds {
        match cmd.run(&mut state).await {
            Ok(ControlFlow::Continue) => (),
            Ok(ControlFlow::Break) => break,
            Err(e) => {
                errors.push(e);
                break;
            }
        }
    }

    if config.reset {
        drop(state);
        if let Err(e) = state_cleanup.await {
            errors.push(anyhow!("cleanup failed: error: {}", e.to_string_with_causes()).into());
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        // Only surface the first error encountered for sake of simplicity
        Err(errors.remove(0))
    }
}
