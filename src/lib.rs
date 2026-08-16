//! Rendering GitHub issues and pull requests as PDFs.
//!
//! The crate is split in two halves, so it can be used either as the whole
//! GitHub App or as a rendering library:
//!
//! - **Always available**: [`pdf`] (Markdown assembly → pandoc → Typst →
//!   PDF), [`models`] (the data the pipeline renders), [`config`] (every
//!   knob that shapes a PDF) and [`description`] (the `gh2pdf` link block).
//! - **Behind the default `server` feature**: [`github`], [`pipeline`] and
//!   [`webhook`] — GitHub App authentication, release publishing, and the
//!   webhook server, along with the `gh2pdf` binary.
//!
//! A consumer that only renders PDFs (inboxbot does) depends on this crate
//! with `default-features = false` and gets none of the server machinery.

pub mod config;
pub mod description;
pub mod models;
pub mod pdf;

#[cfg(feature = "server")]
pub mod github;
#[cfg(feature = "server")]
pub mod pipeline;
#[cfg(feature = "server")]
pub mod webhook;
