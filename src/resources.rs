//! Instance compute requests inherit defaults and stay within administrator limits.
use crate::{config::Profile, error::Error};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
}

impl Resources {
    pub fn is_empty(&self) -> bool {
        self.vcpus.is_none() && self.memory_mib.is_none()
    }

    pub fn effective(&self, profile: &Profile) -> Self {
        Self {
            vcpus: Some(self.vcpus.unwrap_or(profile.vcpus)),
            memory_mib: Some(self.memory_mib.unwrap_or(profile.memory_mib)),
        }
    }

    pub fn resolve(&self, profile: &Profile) -> Result<Profile, Error> {
        let mut resolved = profile.clone();
        resolved.vcpus = self.vcpus.unwrap_or(profile.vcpus);
        resolved.memory_mib = self.memory_mib.unwrap_or(profile.memory_mib);
        let code = if !valid_cpu(resolved.vcpus) {
            Some("INVALID_RESOURCE_VCPUS")
        } else if !(128..=65536).contains(&resolved.memory_mib) {
            Some("INVALID_RESOURCE_MEMORY_MIB")
        } else if resolved.vcpus > profile.max_vcpus.unwrap_or(profile.vcpus) {
            Some("RESOURCE_VCPUS_NOT_AUTHORIZED")
        } else if resolved.memory_mib > profile.max_memory_mib.unwrap_or(profile.memory_mib) {
            Some("RESOURCE_MEMORY_NOT_AUTHORIZED")
        } else {
            None
        };
        if let Some(code) = code {
            return Err(Error::new(StatusCode::BAD_REQUEST, code, "resource_validate"));
        }
        Ok(resolved)
    }
}

fn valid_cpu(value: u8) -> bool {
    (1..=32).contains(&value) && (value == 1 || value.is_multiple_of(2))
}

pub fn validate_limits(profile: &Profile) -> anyhow::Result<()> {
    let cpu = profile.max_vcpus.unwrap_or(profile.vcpus);
    let memory = profile.max_memory_mib.unwrap_or(profile.memory_mib);
    anyhow::ensure!(valid_cpu(cpu) && cpu >= profile.vcpus, "invalid max_vcpus");
    anyhow::ensure!((profile.memory_mib..=65536).contains(&memory), "invalid max_memory_mib");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> Profile {
        let config: crate::config::Config =
            toml::from_str(include_str!("../config.example.toml")).unwrap();
        config.profiles["debian13"].clone()
    }
    #[test]
    fn omitted_and_partial_requests_inherit_each_default() {
        let p = profile();
        let inherited = Resources::default().resolve(&p).unwrap();
        assert_eq!((inherited.vcpus, inherited.memory_mib), (p.vcpus, p.memory_mib));
        let mut p = p;
        p.max_memory_mib = Some(4096);
        let request = Resources { memory_mib: Some(4096), ..Default::default() };
        let resolved = request.resolve(&p).unwrap();
        assert_eq!((resolved.vcpus, resolved.memory_mib), (p.vcpus, 4096));
        assert_eq!(p.memory_mib, 1024);
    }
    #[test]
    fn invalid_and_unauthorized_requests_have_distinct_errors() {
        let p = profile();
        for (cpu, memory, expected) in [
            (3, 1024, "INVALID_RESOURCE_VCPUS"),
            (0, 1024, "INVALID_RESOURCE_VCPUS"),
            (2, 127, "INVALID_RESOURCE_MEMORY_MIB"),
            (2, 65537, "INVALID_RESOURCE_MEMORY_MIB"),
            (4, 1024, "RESOURCE_VCPUS_NOT_AUTHORIZED"),
            (2, 4096, "RESOURCE_MEMORY_NOT_AUTHORIZED"),
        ] {
            let request = Resources { vcpus: Some(cpu), memory_mib: Some(memory) };
            assert_eq!(request.resolve(&p).err().unwrap().code, expected);
        }
    }
    #[test]
    fn administrator_limits_cannot_undercut_defaults_or_exceed_platform_limits() {
        let mut p = profile();
        for limit in [0, 3, 34] {
            p.max_vcpus = Some(limit);
            assert!(validate_limits(&p).is_err());
        }
        p.max_vcpus = Some(4);
        for limit in [128, 65537] {
            p.max_memory_mib = Some(limit);
            assert!(validate_limits(&p).is_err());
        }
        p.max_memory_mib = Some(4096);
        validate_limits(&p).unwrap();
    }
}
