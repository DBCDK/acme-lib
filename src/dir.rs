//
use crate::acc::AcmeKey;
use crate::api::{ApiAccount, ApiDirectory};
use crate::jwt::make_jws_jwk;
use crate::persist::{Persist, PersistKey, PersistKind};
use crate::util::{expect_header, read_json, retry_call};
use crate::{Account, Result};

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
#[derive(Debug, Clone)]
pub struct Directory<P: Persist>(P, ApiDirectory);

impl<P: Persist> Directory<P> {
    /// Create a directory over a persistence implementation and directory url.
    pub fn from_url(persist: P, url: DirectoryUrl) -> Result<Directory<P>> {
        let dir_url = url.to_url();
        let res = retry_call(|| Ok((ureq::get(dir_url), None)))?;
        let api_dir: ApiDirectory = read_json(res)?;
        Ok(Directory(persist, api_dir))
    }

    /// Access an account identified by a contact email.
    ///
    /// If a persisted private key exists for the contact email, it will be read
    /// and used for further access. This way we reuse the same ACME API account.
    ///
    /// If one doesn't exist, it is created and the corresponding public key is
    /// uploaded to the ACME API thus creating the account.
    ///
    /// Either way the `newAccount` API endpoint is called and thereby ensures the
    /// account is active and working.
    pub fn account(&self, contact_email: &str) -> Result<Account<P>> {
        // key in persistence for acme account private key
        let pem_key = PersistKey::new(&contact_email, PersistKind::PrivateKey, "acme_account");

        // Get the key from a saved PEM, or from creating a new
        let mut is_new = false;
        let pem = self.persist().get(&pem_key)?;
        let mut acme_key = if let Some(pem) = pem {
            // we got a persisted private key. read it.
            debug!("Read persisted acme account key");
            AcmeKey::from_pem(&pem)?
        } else {
            // create a new key (and new account)
            debug!("Create new acme account key");
            is_new = true;
            AcmeKey::new()
        };

        // Prepare making a call to newAccount. This is fine to do both for
        // new keys and existing. For existing the spec says to return a 200
        // with the Location header set to the key id (kid).
        let acc = ApiAccount {
            contact: vec![format!("mailto:{}", contact_email)],
            termsOfServiceAgreed: Some(true),
            ..Default::default()
        };
        let res = retry_call(|| {
            let nonce = self.new_nonce()?;
            let url = &self.1.newAccount;
            let body = make_jws_jwk(url, nonce, &acme_key, &acc)?;
            debug!("Call new account endpoint: {}", url);
            let mut req = ureq::post(url);
            req.set("content-type", "application/jose+json");
            Ok((req, Some(body)))
        })?;
        let kid = expect_header(&res, "location")?;
        debug!("Key id is: {}", kid);
        let api_account: ApiAccount = read_json(res)?;

        // fill in the server returned key id
        acme_key.set_key_id(kid);

        // If we did create a new key, save it back to the persistence.
        if is_new {
            debug!("Persist acme account key");
            let pem = acme_key.to_pem();
            self.persist().put(&pem_key, &pem)?;
        }
        // The finished account
        Ok(Account::new(
            self.clone(),
            contact_email,
            acme_key,
            api_account,
        ))
    }

    pub(crate) fn new_nonce(&self) -> Result<String> {
        debug!("Get new nonce");
        let res = retry_call(|| Ok((ureq::head(&self.1.newNonce), None)))?;
        expect_header(&res, "replay-nonce")
    }

    /// Access the underlying JSON object for debugging.
    pub fn api_directory(&self) -> &ApiDirectory {
        &self.1
    }

    pub(crate) fn persist(&self) -> &P {
        &self.0
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::persist::*;
    #[test]
    fn test_create_directory() -> Result<()> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let persist = MemoryPersist::new();
        let _ = Directory::from_url(persist, url)?;
        Ok(())
    }

    #[test]
    fn test_create_acount() -> Result<()> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let persist = MemoryPersist::new();
        let dir = Directory::from_url(persist, url)?;
        let _ = dir.account("foo@bar.com")?;
        Ok(())
    }

    #[test]
    fn test_persisted_acount() -> Result<()> {
        let server = crate::test::with_directory_server();
        let url = DirectoryUrl::Other(&server.dir_url);
        let persist = MemoryPersist::new();
        let dir = Directory::from_url(persist, url)?;
        let acc1 = dir.account("foo@bar.com")?;
        let acc2 = dir.account("foo@bar.com")?;
        let acc3 = dir.account("karlfoo@bar.com")?;
        assert_eq!(acc1.acme_private_key_pem(), acc2.acme_private_key_pem());
        assert!(acc1.acme_private_key_pem() != acc3.acme_private_key_pem());
        Ok(())
    }

    // #[test]
    // fn test_the_whole_hog() -> Result<()> {
    //     ::std::env::set_var("RUST_LOG", "acme_lib=trace");
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