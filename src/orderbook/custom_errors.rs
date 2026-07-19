use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub struct QuantityError {
    pub message: String,
}

impl fmt::Display for QuantityError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Value error: {}", self.message)
    }
}

impl Error for QuantityError {}
