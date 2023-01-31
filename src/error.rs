use std::fmt;

#[derive(Debug)]
pub enum MostroError {
    ParsingInvoiceError,
    ParsingNumberError,
    InvoiceExpiredError,
    MinExpirationTimeError,
    MinAmountError,
}

impl std::error::Error for MostroError {}

impl fmt::Display for MostroError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MostroError::ParsingInvoiceError => write!(f, "Incorrect invoice"),
            MostroError::ParsingNumberError => write!(f, "Error parsing the number"),
            MostroError::InvoiceExpiredError => write!(f, "Invoice has expired"),
            MostroError::MinExpirationTimeError => write!(f, "Minimal expiration time on invoice"),
            MostroError::MinAmountError => write!(f, "Minimal payment amount"),
        }
    }
}

impl From<lightning_invoice::ParseError> for MostroError {
    fn from(_: lightning_invoice::ParseError) -> Self {
        MostroError::ParsingInvoiceError
    }
}

impl From<lightning_invoice::ParseOrSemanticError> for MostroError {
    fn from(_: lightning_invoice::ParseOrSemanticError) -> Self {
        MostroError::ParsingInvoiceError
    }
}

impl From<std::num::ParseIntError> for MostroError {
    fn from(_: std::num::ParseIntError) -> Self {
        MostroError::ParsingNumberError
    }
}
