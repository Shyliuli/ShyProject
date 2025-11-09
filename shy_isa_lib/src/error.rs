#[derive(Debug)]
pub enum CoreError {
    InvalidInput(String),
    ResourceNotFound(String),
    IOError(String),
    MemoryError(String),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            CoreError::InvalidInput(detail) => {
                if detail.is_empty() {
                    write!(f, "The provided input was invalid.")
                } else {
                    write!(f, "The provided input was invalid: {}.", detail)
                }
            }
            CoreError::ResourceNotFound(detail) => {
                if detail.is_empty() {
                    write!(f, "The required resource was not found.")
                } else {
                    write!(f, "The required resource was not found: {}.", detail)
                }
            }
            CoreError::IOError(detail) => {
                if detail.is_empty() {
                    write!(f, "An IO error occurred.")
                } else {
                    write!(f, "An IO error occurred: {}.", detail)
                }
            }
            CoreError::MemoryError(detail) => {
                if detail.is_empty() {
                    write!(f, "A memory error occurred.")
                } else {
                    write!(f, "A memory error occurred: {}.", detail)
                }
            }
        }
    }
}

impl std::error::Error for CoreError {}