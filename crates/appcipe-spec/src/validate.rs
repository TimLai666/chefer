use crate::types::*;

impl AppCipe {
    pub fn validate(&self) -> Result<(), String> {
        match self.version.as_str() {
            "0.1" => Ok(()),
            other => Err(format!("Unsupported version: {}", other)),
        }
    }

    pub fn apply_defaults(mut self) -> Self {
        if self.crash.is_none() {
            self.crash = Some(CrashPolicy::default());
        }

        // 各服務的預設值
        for service in self.services.values_mut() {
            if service.interface_mode.is_none() {
                service.interface_mode = Some(InterfaceMode::default());
            }
            if service.ports.is_none() {
                service.ports = Some(Vec::new());
            }
        }

        self
    }
}
