//! Per-tab UI rendering. One module per tab.
//!
//! Each `draw` is a pure function over `&mut egui::Ui` + form state —
//! no app-wide state pulled in (other than what each tab actually
//! needs). Keeps the App::update method short and the diff for adding
//! a new tab small.

pub mod advanced;
pub mod broker;
pub mod discord;
pub mod hooks;
pub mod orchestrator;
