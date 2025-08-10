use crate::types::*;

impl AppCipe {
    pub fn validate(&self) -> Result<(), String> {
        match self.version.as_str() {
            "0.1" => Ok(()),
            other => Err(format!("Unsupported version: {}", other)),
        }
    }
}
