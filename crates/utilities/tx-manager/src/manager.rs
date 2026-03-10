//! Core transaction manager implementation.

use crate::TxManager;

/// Default transaction manager implementation.
#[derive(Debug)]
pub struct SimpleTxManager;

impl TxManager for SimpleTxManager {}
