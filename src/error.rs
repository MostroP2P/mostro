use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum MostroError {
    ParsingInvoiceError,
    ParsingNumberError,
    InvoiceExpiredError,
    MinExpirationTimeError,
    MinAmountError,
    WrongAmountError,
    NoAPIResponse,
    NoCurrency,
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
            MostroError::WrongAmountError => write!(f, "The amount on this invoice is wrong"),
            MostroError::NoAPIResponse => write!(f, "Price API not answered - retry"),
            MostroError::NoCurrency => write!(f, "Currency requested is not present in the exchange list, please specify a fixed rate"),
        }
    }
}

impl From<lightning_invoice::Bolt11ParseError> for MostroError {
    fn from(_: lightning_invoice::Bolt11ParseError) -> Self {
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

impl From<reqwest::Error> for MostroError {
    fn from(_: reqwest::Error) -> Self {
        MostroError::NoAPIResponse
    }
}
