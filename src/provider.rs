use std::net::IpAddr;

use crate::{
    boil::BoilClient,
    config::{ServerConfig, ServerProvider},
    fachost::{FacHostError, FacHostSeednetClient},
    pqs::{PqsClient, PqsError},
    tcscloud::{TcscloudError, TcscloudSeednetClient},
};

pub struct ProviderClients {
    boil: BoilClient,
    fachost_seednet: FacHostSeednetClient,
    pqs: PqsClient,
    tcscloud_seednet: TcscloudSeednetClient,
}

#[derive(Debug)]
pub enum ProviderError {
    Boil(crate::boil::BoilApiError),
    FacHost(FacHostError),
    Pqs(PqsError),
    Tcscloud(TcscloudError),
    InvalidConfiguration(&'static str),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Boil(error) => error.fmt(f),
            Self::FacHost(error) => error.fmt(f),
            Self::Pqs(error) => error.fmt(f),
            Self::Tcscloud(error) => error.fmt(f),
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
            pqs: PqsClient::new()?,
            tcscloud_seednet: TcscloudSeednetClient,
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
            ServerProvider::Pqs => {
                let config = server
                    .pqs
                    .as_ref()
                    .ok_or(ProviderError::InvalidConfiguration(
                        "缺少 pqs Provider 配置",
                    ))?;
                self.pqs.get_ip(config).await.map_err(ProviderError::Pqs)
            }
            ServerProvider::TcscloudSeednet => {
                let config =
                    server
                        .tcscloud_seednet
                        .as_ref()
                        .ok_or(ProviderError::InvalidConfiguration(
                            "缺少 tcscloud_seednet Provider 配置",
                        ))?;
                self.tcscloud_seednet
                    .get_ip(config)
                    .await
                    .map_err(ProviderError::Tcscloud)
            }
        }
    }

    pub(crate) fn boil(&self) -> &BoilClient {
        &self.boil
    }

    pub(crate) fn fachost_seednet(&self) -> &FacHostSeednetClient {
        &self.fachost_seednet
    }

    pub(crate) fn pqs(&self) -> &PqsClient {
        &self.pqs
    }

    pub(crate) fn tcscloud_seednet(&self) -> &TcscloudSeednetClient {
        &self.tcscloud_seednet
    }

    #[cfg(test)]
    pub(crate) fn with_pqs_api_base_url(api_base_url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            boil: BoilClient::new()?,
            fachost_seednet: FacHostSeednetClient,
            pqs: PqsClient::with_api_base_url(api_base_url)?,
            tcscloud_seednet: TcscloudSeednetClient,
        })
    }
}
