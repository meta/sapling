/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]

mod app;
pub mod args;
mod builder;
mod extension;
pub mod monitoring;
mod pager;
mod repos_manager;
#[cfg(fbcode_build)]
mod server_status;

pub use app::MononokeApp;
pub use builder::MononokeAppBuilder;
pub use extension::AppExtension;
pub use repos::MononokeService;
pub use repos_manager::MononokeReposManager;
pub use running::ShutdownGracePeriod;
#[cfg(fbcode_build)]
pub use server_status::MononokeAppServerStatus;

#[doc(hidden)]
pub mod macro_export {
    pub use base_app;
}

/// Define subcommands for a Mononoke App
///
/// This macro is a convenience macro for defining a Mononoke App with
/// subcommands, where each subcommand gets its own module.
///
/// To use this:
///
/// * Create `commands.rs` with a call to this macro.  List each module
///   that defines a subcommand, which should be placed in a `commands`
///   directory.
///
/// ```text
/// use mononoke_app::subcommands;
///
/// subcommands! {
///    mod first_command;
///    mod second_command;
/// }
/// ```
///
/// * Each command should provide two things at the root of its module:
///
///   * A `CommandArgs` type which should derive `clap::Parser` and represents
///     the options available to this subcommand.
///
///   * A function to run the command, with the signature:
///     `pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()>`.
///
/// * This macro gathers these together and provides a `subcommands` function
///   and a `dispatch` function.
///
///   * Call `commands::subcommands()` to construct the subcommands and pass
///     them to `MononokeAppBuilder::build_with_subcommands` for your app.
///
///   * When executing the app, call `commands::dispatch(app)` to run the
///     selected subcommand.
#[macro_export]
macro_rules! subcommands {
    ( $( mod $command:ident; )* ) => {
        $crate::macro_export::base_app::subcommands!{ type App = $crate::MononokeApp; $( mod $command; )* }
    }
}
