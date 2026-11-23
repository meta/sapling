/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use metaconfig_types::Identity;
use metaconfig_types::P4ServerConfig;
use repos::RawAllowlistIdentity;
use repos::RawP4ReadFrom;
use repos::RawP4ServerConfig;

use crate::convert::Convert;
use crate::errors::ConfigurationError;

impl Convert for RawAllowlistIdentity {
    type Output = Identity;

    fn convert(self) -> Result<Self::Output> {
        if self.identity_type.is_empty() || self.identity_data.is_empty() {
            return Err(ConfigurationError::InvalidFileStructure(
                "identity type and data must be specified".into(),
            )
            .into());
        }
        Ok(Identity {
            id_type: self.identity_type,
            id_data: self.identity_data,
        })
    }
}

impl Convert for RawP4ServerConfig {
    type Output = P4ServerConfig;

    fn convert(self) -> Result<Self::Output> {
        let read_from = self.mirror.read_from;
        let host = match read_from {
            RawP4ReadFrom::PRIMARY => self.primary,
            RawP4ReadFrom::STANDBY => self.standby,
            other => bail!("mirror.read_from must be PRIMARY or STANDBY, not {other}"),
        }
        .with_context(|| format!("mirror.read_from is {read_from} but that host is unset"))?;
        for (field, value) in [
            ("host", &host.host),
            ("fingerprint", &host.fingerprint),
            ("mirror.user", &self.mirror.user),
            ("mirror.ticket_secret", &self.mirror.ticket_secret),
        ] {
            ensure!(!value.is_empty(), "{field} must not be empty");
        }
        Ok(P4ServerConfig {
            host: host.host,
            fingerprint: host.fingerprint,
            user: self.mirror.user,
            ticket_secret: self.mirror.ticket_secret,
        })
    }
}
