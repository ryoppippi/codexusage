#![deny(missing_docs)]
#![deny(rustdoc::missing_crate_level_docs)]
#![deny(clippy::pedantic)]
#![deny(clippy::missing_docs_in_private_items)]
#![deny(clippy::missing_errors_doc)]
#![deny(clippy::missing_panics_doc)]
#![deny(clippy::missing_assert_message)]
#![deny(clippy::missing_asserts_for_indexing)]
#![deny(clippy::unwrap_used)]

//! CLI entrypoint for codexusage.

fn main() -> Result<(), color_eyre::Report> {
    color_eyre::install()?;
    codexusage::app::run(std::env::args_os())?;
    Ok(())
}
