//! nirisaver — a Wayland screensaver for niri.
//!
//! The program is a thin shell around four ideas, and the module boundaries
//! are drawn so each can be tested without the next:
//!
//!   * [`config`] resolves the environment into a plain struct, once, and is
//!     the only module that reads anything;
//!   * [`engine`] drives ttfx and hands out complete frames;
//!   * [`vt`] turns those frames into a [`grid::Grid`] of cells;
//!   * [`render`] turns a new grid into pixels and a damage report, and
//!     [`wayland`] puts that on screen.
//!
//! Everything above `wayland` is compositor-free, which is why [`headless`]
//! and the benchmark can exercise the interesting parts in CI.

pub mod cli;
pub mod config;
pub mod engine;
pub mod grid;
pub mod headless;
pub mod raster;
pub mod render;
pub mod text;
pub mod vt;
pub mod wayland;
