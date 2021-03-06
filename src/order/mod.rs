//! Order life cycle.
//!
//! An order goes through a life cycle of different states that require various actions by
//! the user. To ensure the user only use appropriate actions, this library have simple façade
//! structs that wraps the actual [`ApiOrder`].
//!
//! 1. First prove ownership:
//!    * [`NewOrder`] -> [`Auth`]* -> [`Challenge`]
//! 2. Then submit CSR and download the cert.
//!    * [`NewOrder`] -> [`CsrOrder`] -> [`CertOrder`]
//!
//! \* Possibly multiple auths.
//!
//! [`ApiOrder`]: ../api/struct.ApiOrder.html
//! [`NewOrder`]: struct.NewOrder.html
//! [`Auth`]: struct.Auth.html
//! [`Challenge`]: struct.Challenge.html
//! [`CsrOrder`]: struct.CsrOrder.html
//! [`CertOrder`]: struct.CertOrder.html
use crate::{
    acc::AccountInner,
    api::{ApiAuth, ApiEmptyString, ApiFinalize, ApiOrder},
    cert::{create_csr, Certificate},
    error,
    util::{base64url, read_json},
};
use openssl::pkey::{self, PKey};
use std::{sync::Arc, time::Duration};

mod auth;

pub use self::auth::{Auth, Challenge};

/// The order wrapped with an outer façade.
pub(crate) struct Order {
    inner: Arc<AccountInner>,
    api_order: ApiOrder,
    url: String,
}

impl Order {
    pub(crate) fn new(inner: &Arc<AccountInner>, api_order: ApiOrder, url: String) -> Self {
        Order {
            inner: inner.clone(),
            api_order,
            url,
        }
    }
}

/// Helper to refresh an order status (POST-as-GET).
pub(crate) async fn refresh_order(
    inner: &Arc<AccountInner>,
    url: String,
    want_status: &'static str,
) -> Result<Order, error::Error> {
    let res = inner.transport.call(&url, &ApiEmptyString).await?;

    // our test rig requires the order to be in `want_status`.
    // api_order_of is different for test compilation
    let api_order = api_order_of(res, want_status).await?;

    Ok(Order {
        inner: inner.clone(),
        api_order,
        url,
    })
}

#[cfg(not(test))]
async fn api_order_of(
    res: crate::req::ReqResult,
    _want_status: &str,
) -> Result<ApiOrder, error::Error> {
    read_json(res).await
}

#[cfg(test)]
// our test rig requires the order to be in `want_status`
async fn api_order_of(
    res: crate::req::ReqResult,
    want_status: &str,
) -> Result<ApiOrder, error::Error> {
    #[allow(clippy::trivial_regex)]
    let re = regex::Regex::new("<STATUS>").unwrap();
    let b = re.replace_all(&res.body, want_status).to_string();
    let api_order: ApiOrder = serde_json::from_str(&b)?;
    Ok(api_order)
}

/// A new order created by [`Account::new_order`].
///
/// An order is created using one or many domains (a primary `CN` and possible multiple
/// alt names). All domains in the order must have authorizations ([confirmed ownership])
/// before the order can progress to submitting a [CSR].
///
/// This order façade provides calls to provide such authorizations and to progress the order
/// when ready.
///
/// The ACME API provider might "remember" for a time that you already own a domain, which
/// means you might not need to prove the ownership every time. Use appropriate methods to
/// first check whether you really need to handle authorizations.
///
/// [`Account::new_order`]: ../struct.Account.html#method.new_order
/// [confirmed ownership]: ../index.html#domain-ownership
/// [CSR]: https://en.wikipedia.org/wiki/Certificate_signing_request
pub struct NewOrder {
    pub(crate) order: Order,
}

impl NewOrder {
    /// Tell if the domains in this order have been authorized.
    ///
    /// This doesn't do any calls against the API. You must manually call [`refresh`].
    ///
    /// In ACME API terms, the order can either be `ready` or `valid`, which both would
    /// mean we have passed the authorization stage.
    ///
    /// [`refresh`]: struct.NewOrder.html#method.refresh
    pub async fn is_validated(&self) -> bool {
        self.order.api_order.is_status_ready() || self.order.api_order.is_status_valid()
    }

    /// If the order [`is_validated`] progress it to a [`CsrOrder`].
    ///
    /// This doesn't do any calls against the API. You must manually call [`refresh`].
    ///
    /// [`is_validated`]: struct.NewOrder.html#method.is_validated
    /// [`CsrOrder`]: struct.CsrOrder.html
    pub async fn confirm_validations(&self) -> Option<CsrOrder> {
        if self.is_validated().await {
            Some(CsrOrder {
                order: Order::new(
                    &self.order.inner,
                    self.order.api_order.clone(),
                    self.order.url.clone(),
                ),
            })
        } else {
            None
        }
    }

    /// Refresh the order state against the ACME API.
    ///
    /// The specification calls this a "POST-as-GET" against the order URL.
    pub async fn refresh(&mut self) -> Result<(), error::Error> {
        let order = refresh_order(&self.order.inner, self.order.url.clone(), "ready").await?;
        self.order = order;
        Ok(())
    }

    /// Provide the authorizations. The number of authorizations will be the same as
    /// the number of domains requests, i.e. at least one (the primary CN), but possibly
    /// more (for alt names).
    ///
    /// If the order includes new domain names that have not been authorized before, this
    /// list might contain a mix of already valid and not yet valid auths.
    pub async fn authorizations(&self) -> Result<Vec<Auth>, error::Error> {
        let mut result = vec![];
        if let Some(authorizations) = &self.order.api_order.authorizations {
            for auth_url in authorizations {
                let res = self
                    .order
                    .inner
                    .transport
                    .call(auth_url, &ApiEmptyString)
                    .await?;
                let api_auth: ApiAuth = read_json(res).await?;
                result.push(Auth::new(&self.order.inner, api_auth, auth_url).await);
            }
        }
        Ok(result)
    }

    /// Access the underlying JSON object for debugging.
    pub async fn api_order(&self) -> &ApiOrder {
        &self.order.api_order
    }
}

/// An order that is ready for a [CSR] submission.
///
/// To submit the CSR is called "finalizing" the order.
///
/// To finalize, the user supplies a private key (from which a public key is derived). This
/// library provides [functions to create private keys], but the user can opt for creating them
/// in some other way.
///
/// This library makes no attempt at validating which key algorithms are used. Unsupported
/// algorithms will show as an error when finalizing the order. It is up to the ACME API
/// provider to decide which key algorithms to support.
///
/// Right now Let's Encrypt [supports]:
///
/// * RSA keys from 2048 to 4096 bits in length
/// * P-256 and P-384 ECDSA keys
///
/// [CSR]: https://en.wikipedia.org/wiki/Certificate_signing_request
/// [functions to create key pairs]: ../index.html#functions
/// [supports]: https://letsencrypt.org/docs/integration-guide/#supported-key-algorithms
pub struct CsrOrder {
    pub(crate) order: Order,
}

impl CsrOrder {
    /// Finalize the order by providing a private key as PEM.
    ///
    /// Once the CSR has been submitted, the order goes into a `processing` status,
    /// where we must poll until the status changes. The `delay` is the
    /// amount of time to wait between each poll attempt.
    ///
    /// This is a convenience wrapper that in turn calls the lower level [`finalize_pkey`].
    ///
    /// [`finalize_pkey`]: struct.CsrOrder.html#method.finalize_pkey
    pub async fn finalize(
        self,
        private_key_pem: &str,
        delay: Duration,
    ) -> Result<CertOrder, error::Error> {
        let pkey_pri = PKey::private_key_from_pem(private_key_pem.as_bytes())?;
        self.finalize_pkey(pkey_pri, delay).await
    }

    /// Lower level finalize call that works directly with the openssl crate structures.
    ///
    /// Creates the CSR for the domains in the order and submit it to the ACME API.
    ///
    /// Once the CSR has been submitted, the order goes into a `processing` status,
    /// where we must poll until the status changes. The `delay` is the
    /// amount of time to wait between each poll attempt.
    pub async fn finalize_pkey(
        self,
        private_key: PKey<pkey::Private>,
        delay: Duration,
    ) -> Result<CertOrder, error::Error> {
        //
        // the domains that we have authorized
        let domains = self.order.api_order.domains();

        // csr from private key and authorized domains.
        let csr = create_csr(&private_key, &domains)?;

        // this is not the same as PEM.
        let csr_der = csr.to_der()?;
        let csr_enc = base64url(&csr_der);
        let finalize = ApiFinalize { csr: csr_enc };

        let inner = self.order.inner;
        let order_url = self.order.url;
        let finalize_url = &self.order.api_order.finalize;

        // if the CSR is invalid, we will get a 4xx code back that
        // bombs out from this retry_call.
        inner.transport.call(finalize_url, &finalize).await?;

        // wait for the status to not be processing.
        // valid -> cert is issued
        // invalid -> the whole thing is off
        let order = wait_for_order_status(&inner, &order_url, delay).await?;

        if !order.api_order.is_status_valid() {
            return Err(error::Error::LetsEncryptError(format!(
                "Order is in status: {:?}",
                order.api_order.status
            )));
        }

        Ok(CertOrder { private_key, order })
    }

    /// Access the underlying JSON object for debugging.
    pub fn api_order(&self) -> &ApiOrder {
        &self.order.api_order
    }
}

async fn wait_for_order_status(
    inner: &Arc<AccountInner>,
    url: &str,
    delay: Duration,
) -> Result<Order, error::Error> {
    loop {
        let order = refresh_order(inner, url.to_string(), "valid").await?;
        if !order.api_order.is_status_processing() {
            return Ok(order);
        }
        tokio::time::sleep(delay).await;
    }
}

/// Order for an issued certificate that is ready to download.
pub struct CertOrder {
    private_key: PKey<pkey::Private>,
    order: Order,
}

impl CertOrder {
    /// Request download of the issued certificate.
    pub async fn download_cert(self) -> Result<Certificate, error::Error> {
        //
        let url = self
            .order
            .api_order
            .certificate
            .ok_or_else(|| error::Error::LetsEncryptError("certificate url".to_string()))?;
        let inner = self.order.inner;

        let res = inner.transport.call(&url, &ApiEmptyString).await?;

        // save key and cert into persistence
        let pkey_pem_bytes = self.private_key.private_key_to_pem_pkcs8()?;
        let pkey_pem = String::from_utf8_lossy(&pkey_pem_bytes);

        let cert = res.body;

        Ok(Certificate::new(pkey_pem.to_string(), cert))
    }

    /// Access the underlying JSON object for debugging.
    pub fn api_order(&self) -> &ApiOrder {
        &self.order.api_order
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::*;

    #[tokio::test]
    async fn test_get_authorizations() -> Result<(), error::Error> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let dir = Directory::from_url(url).await?;
        let acc = dir
            .register_account(vec!["mailto:foo@bar.com".to_string()])
            .await?;
        let ord = acc.new_order("acmetest.example.com", &[]).await?;
        let _ = ord.authorizations().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_finalize() -> Result<(), error::Error> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let dir = Directory::from_url(url).await?;
        let acc = dir
            .register_account(vec!["mailto:foo@bar.com".to_string()])
            .await?;
        let ord = acc.new_order("acmetest.example.com", &[]).await?;
        // shortcut auth
        let ord = CsrOrder { order: ord.order };
        let pkey = cert::create_p256_key()?;
        let _ord = ord.finalize_pkey(pkey, Duration::from_millis(1)).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_download_and_save_cert() -> Result<(), error::Error> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let dir = Directory::from_url(url).await?;
        let acc = dir
            .register_account(vec!["mailto:foo@bar.com".to_string()])
            .await?;
        let ord = acc.new_order("acmetest.example.com", &[]).await?;

        // shortcut auth
        let ord = CsrOrder { order: ord.order };
        let pkey = cert::create_p256_key()?;
        let ord = ord.finalize_pkey(pkey, Duration::from_millis(1)).await?;

        let cert = ord.download_cert().await?;
        assert_eq!("CERT HERE", cert.certificate());
        assert!(!cert.private_key().is_empty());
        let test_expiry = chrono::DateTime::<chrono::Utc>::from_utc(chrono::NaiveDateTime::parse_from_str("May 15 11:11:11 2015 GMT", "%h %e %H:%M:%S %Y GMT")?, chrono::Utc);
        assert_eq!(
            cert.expiry()?,
            test_expiry
        );
        
    
        Ok(())
    }
}
