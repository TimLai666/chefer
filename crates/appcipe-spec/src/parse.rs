use crate::types::AppCipe;
use std::fs;

pub fn from_file(path: &str) -> Result<AppCipe, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    from_str(&content)
}

pub fn from_str(yaml: &str) -> Result<AppCipe, Box<dyn std::error::Error>> {
    let app: AppCipe = serde_yaml::from_str(yaml)?;
    Ok(app)
}
