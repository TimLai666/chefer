use crate::types::AppCipe;
use std::fs;

pub fn from_file(path: &str) -> Result<AppCipe, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    from_str(&content)
}

pub fn from_str(yaml: &str) -> Result<AppCipe, Box<dyn std::error::Error>> {
    let mut app: AppCipe = serde_yaml::from_str(yaml)?;
    // 驗證
    app.validate()
        .map_err(|e| format!("Validation error: {}", e))?;

    // 套預設
    app = app.apply_defaults();

    Ok(app)
}
