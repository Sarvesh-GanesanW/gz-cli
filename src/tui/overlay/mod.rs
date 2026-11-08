//! Small dialogs drawn over everything: the action menu, confirmations and
//! forms. Each is a pure state machine fed keys; the app decides what a
//! finished dialog does.

pub mod confirm;
pub mod form;
pub mod menu;
