use std::net::IpAddr;

use crate::{
    boil::BoilClient,
    config::{ServerConfig, ServerProvider},
    fachost::{FacHostError, FacHostSeednetClient},
};

pub struct ProviderClients {
    boil: BoilClient,
    fachost_seednet: FacHostSeednetClient,
}

#[derive(Debug)]
pub enum ProviderError {
    Boil(crate::boil::BoilApiError),
    FacHost(FacHostError),
    InvalidConfiguration(&'static str),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Boil(error) => error.fmt(f),
            Self::FacHost(error) => error.fmt(f),
            Self::InvalidConfiguration(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ProviderError {}

impl ProviderClients {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            boil: BoilClient::new()?,
            fachost_seednet: FacHostSeednetClient,
        })
    }

    pub async fn get_ip(&self, server: &ServerConfig) -> Result<IpAddr, ProviderError> {
        match server.provider {
            ServerProvider::Boil => self
                .boil
                .get_ip(&server.token)
                .await
                .map(|response| response.ip)
                .map_err(ProviderError::Boil),
            ServerProvider::FachostSeednet => {
                let config =
                    server
                        .fachost_seednet
                        .as_ref()
                        .ok_or(ProviderError::InvalidConfiguration(
                            "缺少 fachost_seednet Provider 配置",
                        ))?;
                self.fachost_seednet
                    .get_ip(config)
                    .await
                    .map_err(ProviderError::FacHost)
            }
        }
    }

    pub(crate) fn boil(&self) -> &BoilClient {
        &self.boil
    }

    pub(crate) fn fachost_seednet(&self) -> &FacHostSeednetClient {
        &self.fachost_seednet
    }
}
