use crate::types::*;

impl AppCipe {
    pub fn validate(&self) -> Result<(), String> {
        match self.version.as_str() {
            "0.1" => {
                // 檢查 name 是否只包含英文或底線且不含空格
                if !self.name.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
                    return Err("name can only contain English letters and underscores, and cannot have spaces".to_string());
                }
                self.services.iter().try_for_each(|(name, _)| {
                    if name.is_empty() {
                        return Err("Service name cannot be empty".to_string());
                    }
                    if name.chars().any(|c| !c.is_ascii_alphanumeric() && c != '_') {
                        return Err(format!("Service name '{}' can only contain alphanumeric characters and underscores", name));
                    }
                    Ok(())
                })?;
                Ok(())
            },
            other => Err(format!("Unsupported version: {}", other)),
        }
    }
}
