use std::fmt;

#[derive(Debug)]
pub enum MostroError {
    ParsingInvoiceError,
}

impl std::error::Error for MostroError {}

impl fmt::Display for MostroError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MostroError::ParsingInvoiceError => write!(f, "Parsing invoice error"),
        }
    }
}

impl From<lightning_invoice::ParseOrSemanticError> for MostroError {
    fn from(_: lightning_invoice::ParseOrSemanticError) -> Self {
        MostroError::ParsingInvoiceError
    }
}
