//
use std::sync::Arc;

use crate::{
    acc::AcmeKey,
    api::{ApiAccount, ApiDirectory},
    error,
    req::{req_expect_header, req_get},
    trans::{NoncePool, Transport},
    util::read_json,
    Account,
};

const LETSENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
const LETSENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Enumeration of known ACME API directories.
#[derive(Debug, Clone)]
pub enum DirectoryUrl<'a> {
    /// The main Let's Encrypt directory. Not appropriate for testing and dev.
    LetsEncrypt,
    /// The staging Let's Encrypt directory. Use for testing and dev. Doesn't issue
    /// "valid" certificates. The root signing certificate is not supposed
    /// to be in any trust chains.
    LetsEncryptStaging,
    /// Provide an arbitrary director URL to connect to.
    Other(&'a str),
}

impl<'a> DirectoryUrl<'a> {
    fn to_url(&self) -> &str {
        match self {
            DirectoryUrl::LetsEncrypt => LETSENCRYPT,
            DirectoryUrl::LetsEncryptStaging => LETSENCRYPT_STAGING,
            DirectoryUrl::Other(s) => s,
        }
    }
}

/// Entry point for accessing an ACME API.
#[derive(Clone)]
pub struct Directory {
    nonce_pool: Arc<NoncePool>,
    api_directory: ApiDirectory,
}

impl Directory {
    /// Create a directory over a persistence implementation and directory url.
    pub async fn from_url(url: DirectoryUrl<'_>) -> Result<Directory, error::Error> {
        let dir_url = url.to_url();
        let res = req_get(&dir_url).await?;
        let api_directory: ApiDirectory = serde_json::from_str(&res.body)?;
        let nonce_pool = Arc::new(NoncePool::new(&api_directory.newNonce).await);
        Ok(Directory {
            nonce_pool,
            api_directory,
        })
    }

    pub async fn register_account(&self, contact: Vec<String>) -> Result<Account, error::Error> {
        let acme_key = AcmeKey::new()?;
        self.upsert_account(acme_key, contact).await
    }

    pub async fn load_account(
        &self,
        pem: &str,
        contact: Vec<String>,
    ) -> Result<Account, error::Error> {
        let acme_key = AcmeKey::from_pem(pem.as_bytes())?;
        self.upsert_account(acme_key, contact).await
    }

    async fn upsert_account(
        &self,
        acme_key: AcmeKey,
        contact: Vec<String>,
    ) -> Result<Account, error::Error> {
        // Prepare making a call to newAccount. This is fine to do both for
        // new keys and existing. For existing the spec says to return a 200
        // with the Location header set to the key id (kid).
        let acc = ApiAccount {
            contact,
            termsOfServiceAgreed: Some(true),
            ..Default::default()
        };

        let mut transport = Transport::new(&self.nonce_pool, acme_key).await;
        let res = transport
            .call_jwk(&self.api_directory.newAccount, &acc)
            .await?;
        let kid = req_expect_header(&res, "location")?;
        debug!("Key id is: {}", kid);
        let api_account: ApiAccount = read_json(res).await?;

        // fill in the server returned key id
        transport.set_key_id(kid).await;

        // The finished account
        Ok(Account::new(
            transport,
            api_account,
            self.api_directory.clone(),
        ))
    }

    /// Access the underlying JSON object for debugging.
    pub fn api_directory(&self) -> &ApiDirectory {
        &self.api_directory
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_create_directory() -> Result<(), error::Error> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let _ = Directory::from_url(url).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_create_acount() -> Result<(), error::Error> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let dir = Directory::from_url(url).await?;
        let _ = dir
            .register_account(vec!["mailto:foo@bar.com".to_string()])
            .await?;
        Ok(())
    }

    // #[test]
    // fn test_the_whole_hog() -> Result<()> {
    //     std::env::set_var("RUST_LOG", "acme_micro=trace");
    //     let _ = env_logger::try_init();

    //     use crate::cert::create_p384_key;

    //     let url = DirectoryUrl::LetsEncryptStaging;
    //     let persist = FilePersist::new(".");
    //     let dir = Directory::from_url(persist, url)?;
    //     let acc = dir.account("foo@bar.com")?;

    //     let mut ord = acc.new_order("myspecialsite.com", &[])?;

    //     let ord = loop {
    //         if let Some(ord) = ord.confirm_validations() {
    //             break ord;
    //         }

    //         let auths = ord.authorizations()?;
    //         let chall = auths[0].dns_challenge();

    //         info!("Proof: {}", chall.dns_proof());

    //         use std::thread;
    //         use std::time::Duration;
    //         thread::sleep(Duration::from_millis(60_000));

    //         chall.validate(5000)?;

    //         ord.refresh()?;
    //     };

    //     let (pkey_pri, pkey_pub) = create_p384_key();

    //     let ord = ord.finalize_pkey(pkey_pri, pkey_pub, 5000)?;

    //     let cert = ord.download_and_save_cert()?;
    //     println!(
    //         "{}{}{}",
    //         cert.private_key(),
    //         cert.certificate(),
    //         cert.valid_days_left()
    //     );
    //     Ok(())
    // }
}
