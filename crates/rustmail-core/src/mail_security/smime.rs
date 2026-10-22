//! S/MIME (CMS) via GnuPG's `gpgsm`. Certificates and private keys stay in
//! GnuPG's keybox and gpg-agent; account settings hold only certificate
//! fingerprints (the SHA-1 shown by `gpgsm --list-keys`).
//!
//! Revocation is not checked: CRL/OCSP lookups would send the certificates a
//! user talks to over the network, and Rustmail never contacts third parties
//! on its own. Chains must still end in a root GnuPG trusts.
use super::{
    MAX_CRYPTO_BYTES, MAX_STATUS_BYTES, MailSecurity, OpenedMessage, decode_colon, fingerprint,
    invalid_signature, multipart, split_entity, valid_signature,
};
use crate::{
    error::{CoreError, Result},
    models::Address,
};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn error(message: &str) -> CoreError {
    CoreError::Other(format!("S/MIME: {message}"))
}

#[derive(Clone, Default)]
pub struct Smime {
    // Test engines use isolated keyboxes. Production uses the user's GnuPG home.
    pub(super) home: Option<PathBuf>,
}

impl Smime {
    async fn run(&self, args: &[String], input: Option<&[u8]>) -> Result<(Vec<u8>, String)> {
        if input.is_some_and(|data| data.len() > MAX_CRYPTO_BYTES) {
            return Err(error("Message exceeds the 100 MiB S/MIME limit"));
        }
        let mut cmd = tokio::process::Command::new("gpgsm");
        cmd.args([
            "--no-options",
            "--batch",
            "--yes",
            "--disable-dirmngr",
            "--disable-crl-checks",
            "--status-fd",
            "2",
        ]);
        if let Some(home) = &self.home {
            cmd.arg("--homedir").arg(home);
        }
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|_| error("Install GnuPG 2.x and make gpgsm available on PATH"))?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let task = async {
            let write = async {
                if let Some(input) = input {
                    // gpgsm may exit before reading everything; its status says why.
                    let _ = stdin.write_all(input).await;
                }
                drop(stdin);
                Ok::<_, std::io::Error>(())
            };
            let read = async {
                let mut data = Vec::new();
                stdout
                    .take((MAX_CRYPTO_BYTES + 1) as u64)
                    .read_to_end(&mut data)
                    .await?;
                Ok::<_, std::io::Error>(data)
            };
            // gpgsm has no status file: status lines share stderr with
            // diagnostics, so keep only the tagged lines.
            let status = async {
                let mut data = Vec::new();
                stderr
                    .take((MAX_STATUS_BYTES + 1) as u64)
                    .read_to_end(&mut data)
                    .await?;
                Ok::<_, std::io::Error>(data)
            };
            // A closed stdin (gpgsm exits early) is not an error by itself.
            let (_, data, status) = tokio::try_join!(write, read, status)?;
            if data.len() > MAX_CRYPTO_BYTES {
                return Err(error("S/MIME output exceeds the size limit"));
            }
            if status.len() > MAX_STATUS_BYTES {
                return Err(error("GnuPG status exceeds the size limit"));
            }
            let exit = child.wait().await?;
            let status = String::from_utf8_lossy(&status)
                .lines()
                .filter(|line| line.starts_with("[GNUPG:] "))
                .collect::<Vec<_>>()
                .join("\n");
            if invalid_signature(&status) {
                return Err(error(
                    "The digital signature is invalid, expired, or revoked",
                ));
            }
            if !exit.success() {
                // Do not expose arbitrary diagnostics (subjects, filenames, user data).
                return Err(error(if status.contains("INV_RECP") {
                    "A recipient certificate is missing, expired, or not trusted"
                } else if status.contains("INV_SGNR") || status.contains("NO_SECKEY") {
                    "The private key is unavailable"
                } else if status.contains("DECRYPTION_FAILED") {
                    "This message could not be decrypted with your private keys"
                } else {
                    "GnuPG could not complete the operation. Check the certificate chain is trusted and unlock the private key using pinentry"
                }));
            }
            Ok((data, status))
        };
        tokio::time::timeout(Duration::from_secs(120), task)
            .await
            .map_err(|_| error("GnuPG timed out; no message was sent"))?
    }

    /// Certificate listing in GnuPG's colon format, validity included.
    pub async fn list_keys(&self, secret: bool) -> Result<String> {
        let (data, _) = self.run(&list_args(secret, None), None).await?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    /// Plain-text certificate inventory for the settings screen.
    pub async fn inventory(&self) -> Result<String> {
        let display = |listing: &str| {
            listing
                .lines()
                .filter_map(|line| {
                    let f: Vec<_> = line.split(':').collect();
                    match f.first().copied()? {
                        "crt" => Some("Certificate".to_owned()),
                        "crs" => Some("Certificate with private key".to_owned()),
                        "fpr" => f.get(9).map(|s| format!("  {s}")),
                        "uid" => f
                            .get(9)
                            .map(|s| decode_colon(s))
                            .filter(|s| s.starts_with('<'))
                            .map(|s| format!("  {s}")),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let secret = self.list_keys(true).await?;
        let public = self.list_keys(false).await?;
        Ok(format!("{}\n\n{}", display(&secret), display(&public)))
    }

    /// PEM/DER certificates and PKCS#12 bundles. A PKCS#12 passphrase is
    /// entered in GnuPG's pinentry, never in this process.
    pub async fn import_key(&self, data: &[u8]) -> Result<()> {
        if data.len() > 1024 * 1024 {
            return Err(error("Certificate files must be smaller than 1 MiB"));
        }
        self.run(&["--import".into()], Some(data)).await?;
        Ok(())
    }

    pub async fn export_certificate(&self, key: &str) -> Result<Vec<u8>> {
        let key = fingerprint(key, false)?;
        let (data, _) = self
            .run(&["--armor".into(), "--export".into(), key], None)
            .await?;
        if data.is_empty() {
            return Err(error("Certificate not found"));
        }
        Ok(data)
    }

    /// Exact certificate fingerprint, a chain GnuPG fully trusts, an email
    /// identity match, and the needed key usage ('s' sign, 'e' encrypt).
    async fn check_cert(&self, key: &str, email: &str, secret: bool, usage: char) -> Result<()> {
        let key = fingerprint(key, false)?;
        let (data, _) = self.run(&list_args(secret, Some(&key)), None).await?;
        validate_listing(&String::from_utf8_lossy(&data), &key, email, usage)
    }

    pub async fn preflight(
        &self,
        policy: &MailSecurity,
        sender: &str,
        recipients: &[Address],
    ) -> Result<()> {
        let mut checked = policy.clone();
        checked.validate()?;
        if !checked.enabled() {
            return Ok(());
        }
        if checked.sign_by_default {
            self.check_cert(&checked.signing_fingerprint, sender, true, 's')
                .await?;
        }
        if checked.require_encryption {
            self.check_cert(&checked.signing_fingerprint, sender, true, 'e')
                .await?;
            for recipient in recipients {
                if recipient.email.eq_ignore_ascii_case(sender) {
                    continue;
                }
                let key = checked
                    .recipient_keys
                    .get(&recipient.email.to_lowercase())
                    .ok_or_else(|| {
                        error(&format!(
                            "Verify a certificate for {} before sending. Encryption is required",
                            recipient.email
                        ))
                    })?;
                self.check_cert(key, &recipient.email, false, 'e').await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn protect(
        &self,
        raw: &[u8],
        policy: &MailSecurity,
        sender: &str,
        recipients: &[Address],
    ) -> Result<Vec<u8>> {
        if !policy.enabled() {
            return Ok(raw.to_vec());
        }
        let mut normalized = policy.clone();
        normalized.validate()?;
        let policy = &normalized;
        self.preflight(policy, sender, recipients).await?;
        let (headers, mut entity) = split_entity(raw)?;
        if policy.require_encryption
            && headers.split(|b| *b == b'\n').any(|line| {
                line.get(..4)
                    .is_some_and(|v| v.eq_ignore_ascii_case(b"bcc:"))
            })
        {
            // CMS names every recipient certificate, so Bcc would be revealed.
            return Err(error(
                "Encrypted S/MIME would reveal Bcc recipients. Move them to To or Cc, or send separately",
            ));
        }
        if policy.sign_by_default {
            let (signature, status) = self
                .run(
                    &[
                        "--base64".into(),
                        "--digest-algo".into(),
                        "SHA256".into(),
                        "--local-user".into(),
                        policy.signing_fingerprint.clone(),
                        "--detach-sign".into(),
                    ],
                    Some(&entity),
                )
                .await?;
            if !status.contains("[GNUPG:] SIG_CREATED ") {
                return Err(error("GnuPG did not confirm the signature"));
            }
            entity = multipart(
                "signed",
                "application/pkcs7-signature",
                Some("sha-256"),
                &entity,
                &part(
                    "application/pkcs7-signature; name=\"smime.p7s\"",
                    "attachment; filename=\"smime.p7s\"",
                    &signature,
                ),
            );
        }
        if policy.require_encryption {
            // The sender's own certificate keeps the Sent copy readable.
            let mut args = vec![
                "--base64".into(),
                "--recipient".into(),
                policy.signing_fingerprint.clone(),
            ];
            for recipient in recipients {
                if let Some(key) = policy.recipient_keys.get(&recipient.email.to_lowercase())
                    && !args.contains(key)
                {
                    args.extend(["--recipient".into(), key.clone()]);
                }
            }
            args.push("--encrypt".into());
            let (ciphertext, _) = self.run(&args, Some(&entity)).await?;
            if ciphertext.is_empty() {
                return Err(error("GnuPG did not produce encrypted data"));
            }
            entity = part(
                "application/pkcs7-mime; smime-type=enveloped-data; name=\"smime.p7m\"",
                "attachment; filename=\"smime.p7m\"",
                &ciphertext,
            );
        }
        Ok([headers, entity].concat())
    }
}

fn list_args(secret: bool, key: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--with-colons".into(),
        "--with-validation".into(),
        if secret {
            "--list-secret-keys"
        } else {
            "--list-keys"
        }
        .into(),
    ];
    args.extend(key.map(str::to_owned));
    args
}

/// A base64 MIME entity with CRLF line ends.
fn part(content_type: &str, disposition: &str, base64: &[u8]) -> Vec<u8> {
    let body = String::from_utf8_lossy(base64)
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\r\n");
    format!(
        "Content-Type: {content_type}\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: {disposition}\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

fn validate_listing(listing: &str, key: &str, email: &str, usage: char) -> Result<()> {
    let mut primary = false;
    let mut exact = false;
    let mut valid = false;
    let mut uid = false;
    for line in listing.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() < 10 {
            continue;
        }
        match fields[0] {
            "crt" | "crs" => {
                primary = true;
                // 'f' = chain fully valid, 'u' = ultimately trusted root.
                valid = matches!(fields[1], "f" | "u")
                    && fields
                        .get(11)
                        .is_some_and(|v| v.to_ascii_lowercase().contains(usage));
            }
            "fpr" if primary => {
                exact |= fields[9].eq_ignore_ascii_case(key);
                primary = false;
            }
            "uid" => {
                let value = decode_colon(fields[9]);
                if let Some(address) = value
                    .strip_prefix('<')
                    .and_then(|v| v.strip_suffix('>'))
                    .filter(|v| !v.contains(['\r', '\n', '\0']))
                {
                    uid |= address.eq_ignore_ascii_case(email);
                }
            }
            _ => {}
        }
    }
    if exact && valid && uid {
        Ok(())
    } else {
        Err(error(
            "Certificate must chain to a trusted root, match the email address, and allow the requested use",
        ))
    }
}

/// True when the message root is S/MIME (enveloped, opaque-signed or detached).
pub(super) fn is_smime(raw: &[u8]) -> bool {
    use mail_parser::MimeHeaders;
    let Some(parsed) = mail_parser::MessageParser::default().parse(raw) else {
        return false;
    };
    let Some(root) = parsed.parts.first() else {
        return false;
    };
    let Some(ct) = root.content_type() else {
        return false;
    };
    let kind = format!(
        "{}/{}",
        ct.ctype().to_ascii_lowercase(),
        ct.subtype().unwrap_or_default().to_ascii_lowercase()
    );
    matches!(
        kind.as_str(),
        "application/pkcs7-mime" | "application/x-pkcs7-mime"
    ) || (kind == "multipart/signed" && ct.attribute("protocol").is_some_and(is_pkcs7_signature))
}

fn is_pkcs7_signature(protocol: &str) -> bool {
    matches!(
        protocol.to_ascii_lowercase().as_str(),
        "application/pkcs7-signature" | "application/x-pkcs7-signature"
    )
}

fn trusted_signature(status: &str) -> Option<String> {
    let trusted = status
        .lines()
        .any(|l| l.starts_with("[GNUPG:] TRUST_FULLY") || l.starts_with("[GNUPG:] TRUST_ULTIMATE"));
    if trusted {
        valid_signature(status)
    } else {
        None
    }
}

impl Smime {
    pub async fn open_message(
        &self,
        raw: &[u8],
        expected_fingerprint: Option<&str>,
    ) -> Result<OpenedMessage> {
        use mail_parser::MimeHeaders;
        if raw.len() > MAX_CRYPTO_BYTES {
            return Err(error("Message exceeds the 100 MiB S/MIME limit"));
        }
        let mut entity = raw.to_vec();
        let mut encrypted = false;
        let mut signature = None;
        for _ in 0..8 {
            let parsed = mail_parser::MessageParser::default()
                .parse(entity.as_slice())
                .ok_or_else(|| error("Malformed S/MIME message"))?;
            let root = parsed
                .parts
                .first()
                .ok_or_else(|| error("Missing MIME part"))?;
            let ct = root.content_type();
            let kind = ct.and_then(|ct| ct.subtype()).unwrap_or_default();
            let smime_type = ct
                .and_then(|ct| ct.attribute("smime-type"))
                .unwrap_or_default();
            let name = root.attachment_name().unwrap_or_default();
            if kind.to_ascii_lowercase().ends_with("pkcs7-mime") {
                let der = root.contents();
                if smime_type.eq_ignore_ascii_case("signed-data") {
                    let out = tempfile::NamedTempFile::new()?;
                    let (_, status) = self
                        .run(
                            &[
                                "--output".into(),
                                out.path().to_string_lossy().into_owned(),
                                "--verify".into(),
                            ],
                            Some(der),
                        )
                        .await?;
                    signature = trusted_signature(&status);
                    if signature.is_none() {
                        return Err(error("The digital signature could not be verified"));
                    }
                    entity = crate::file_io::read(
                        out.path().to_owned(),
                        MAX_CRYPTO_BYTES,
                        "S/MIME payload",
                    )
                    .await?;
                    break;
                }
                if smime_type.eq_ignore_ascii_case("enveloped-data")
                    || (smime_type.is_empty() && name.to_ascii_lowercase().ends_with(".p7m"))
                {
                    let (data, status) = self.run(&["--decrypt".into()], Some(der)).await?;
                    let has = |tag: &str| status.lines().any(|l| l == format!("[GNUPG:] {tag}"));
                    if !has("DECRYPTION_OKAY") || has("DECRYPTION_FAILED") {
                        return Err(error("The message could not be decrypted"));
                    }
                    encrypted = true;
                    entity = data;
                    continue;
                }
                return Err(error("Unsupported S/MIME content type"));
            }
            if kind.eq_ignore_ascii_case("signed")
                && ct
                    .and_then(|ct| ct.attribute("protocol"))
                    .is_some_and(is_pkcs7_signature)
            {
                let parts = root
                    .sub_parts()
                    .filter(|p| p.len() == 2)
                    .ok_or_else(|| error("Signed MIME must have two parts"))?;
                let signed = &parsed.parts[parts[0] as usize];
                let data = &entity[signed.offset_header as usize..signed.offset_end as usize];
                let sig = &parsed.parts[parts[1] as usize];
                let file = tempfile::NamedTempFile::new()?;
                tokio::fs::write(file.path(), sig.contents()).await?;
                let (_, status) = self
                    .run(
                        &[
                            "--verify".into(),
                            file.path().to_string_lossy().into_owned(),
                            "-".into(),
                        ],
                        Some(data),
                    )
                    .await?;
                signature = trusted_signature(&status);
                if signature.is_none() {
                    return Err(error("The digital signature could not be verified"));
                }
                entity = data.to_vec();
                break;
            }
            break;
        }
        if !encrypted && signature.is_none() {
            return Err(error("No supported S/MIME content found"));
        }
        let parsed = mail_parser::MessageParser::default()
            .parse(entity.as_slice())
            .ok_or_else(|| error("Decrypted MIME could not be parsed"))?;
        let text = parsed
            .body_text(0)
            .map(|s| s.into_owned())
            .unwrap_or_default();
        let attachment_count = parsed.attachments().count();
        let protection = match (encrypted, signature.is_some()) {
            (true, true) => "Decrypted.",
            (true, false) => {
                "Decrypted. S/MIME encryption alone does not prove the content is unmodified."
            }
            _ => "Not encrypted.",
        };
        let signature_status = match signature {
            Some(fpr)
                if expected_fingerprint
                    .is_some_and(|expected| expected.eq_ignore_ascii_case(&fpr)) =>
            {
                format!("Valid signature from the verified certificate {fpr}.")
            }
            Some(fpr) => format!(
                "Valid signature from certificate {fpr}. The sender's identity has not been verified against this certificate."
            ),
            None => "No verified digital signature; sender identity is unverified.".to_owned(),
        };
        Ok(OpenedMessage {
            text,
            status: format!(
                "{protection} {signature_status} {attachment_count} attachment(s); export the decrypted message to open them. Outer subject and addresses are not authenticated by this signature. Revocation was not checked."
            ),
            mime: entity,
        })
    }
}

#[cfg(test)]
mod tests {
    // The OpenSSL/gpgsm fixtures are only used by the unix tests.
    #![cfg_attr(not(unix), allow(dead_code, unused_imports))]
    use super::*;
    use std::{collections::BTreeMap, path::Path, process::Command};

    fn openssl(dir: &Path, args: &[&str]) -> Option<String> {
        let out = Command::new("openssl")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// A CA and a PKCS#12 user bundle for `name`; returns (bundle path, cert SHA-1).
    fn issue(dir: &Path, ca: &str, name: &str) -> Option<(PathBuf, String)> {
        let email = format!("{name}@example.org");
        let subject = format!("/CN={name}/emailAddress={email}");
        let ext = format!(
            "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=emailProtection\nsubjectAltName=email:{email}\n"
        );
        std::fs::write(dir.join(format!("{name}.ext")), ext).ok()?;
        let key = format!("{name}.key");
        let csr = format!("{name}.csr");
        let pem = format!("{name}.pem");
        let p12 = format!("{name}.p12");
        openssl(
            dir,
            &[
                "req", "-newkey", "rsa:2048", "-nodes", "-keyout", &key, "-out", &csr, "-subj",
                &subject,
            ],
        )?;
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &csr,
                "-CA",
                &format!("{ca}.pem"),
                "-CAkey",
                &format!("{ca}.key"),
                "-CAcreateserial",
                "-out",
                &pem,
                "-days",
                "2",
                "-extfile",
                &format!("{name}.ext"),
            ],
        )?;
        // The rogue issuer is left out of its bundle: GnuPG can never build a
        // chain to a root it trusts, and no trust prompt is triggered.
        let ca_pem = format!("{ca}.pem");
        let mut args = vec!["pkcs12", "-export", "-inkey", &key, "-in", &pem];
        if ca == "trusted-root" {
            args.extend(["-certfile", &ca_pem]);
        }
        args.extend(["-out", &p12, "-passout", "pass:pw"]);
        openssl(dir, &args)?;
        let fpr = openssl(
            dir,
            &["x509", "-in", &pem, "-noout", "-fingerprint", "-sha1"],
        )?;
        Some((
            dir.join(p12),
            fpr.split('=').nth(1)?.replace(':', "").trim().to_owned(),
        ))
    }

    fn ca(dir: &Path, name: &str) -> Option<String> {
        openssl(
            dir,
            &[
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.pem"),
                "-days",
                "2",
                "-subj",
                &format!("/CN={name}"),
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
            ],
        )?;
        let fpr = openssl(
            dir,
            &[
                "x509",
                "-in",
                &format!("{name}.pem"),
                "-noout",
                "-fingerprint",
                "-sha1",
            ],
        )?;
        Some(fpr.split('=').nth(1)?.replace(':', "").trim().to_owned())
    }

    struct Fixture {
        smime: Smime,
        alice: String,
        bob: String,
        mallory: String,
        _home: tempfile::TempDir,
        _work: tempfile::TempDir,
    }

    /// Requires GnuPG's gpgsm and the openssl CLI; never touches the
    /// developer's keybox. Returns None (test skipped) when a tool is missing.
    #[cfg(unix)]
    async fn fixture() -> Option<Fixture> {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("gpgsm").arg("--version").output().is_err() {
            eprintln!("skipped: gpgsm not installed");
            return None;
        }
        let work = tempfile::tempdir().unwrap();
        let Some(root) = ca(work.path(), "trusted-root") else {
            eprintln!("skipped: openssl not available");
            return None;
        };
        ca(work.path(), "rogue-root").unwrap();
        let (alice_p12, alice) = issue(work.path(), "trusted-root", "alice").unwrap();
        let (bob_p12, bob) = issue(work.path(), "trusted-root", "bob").unwrap();
        let (mallory_p12, mallory) = issue(work.path(), "rogue-root", "mallory").unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // PKCS#12 passphrases go through pinentry, as in production.
        let pinentry = home.path().join("test-pinentry");
        std::fs::write(&pinentry, "#!/bin/sh\nprintf 'OK ready\\n'\nwhile IFS= read -r line; do\n case \"$line\" in\n GETPIN*) printf 'D pw\\nOK\\n' ;;\n BYE*) printf 'OK\\n'; exit 0 ;;\n *) printf 'OK\\n' ;;\n esac\ndone\n").unwrap();
        std::fs::set_permissions(&pinentry, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            home.path().join("gpg-agent.conf"),
            format!("pinentry-program {}\n", pinentry.display()),
        )
        .unwrap();
        std::fs::write(
            home.path().join("trustlist.txt"),
            format!("{root} S relax\n"),
        )
        .unwrap();
        let smime = Smime {
            home: Some(home.path().to_owned()),
        };
        for p12 in [alice_p12, bob_p12, mallory_p12] {
            smime
                .import_key(&std::fs::read(p12).unwrap())
                .await
                .unwrap();
        }
        Some(Fixture {
            smime,
            alice,
            bob,
            mallory,
            _home: home,
            _work: work,
        })
    }

    fn bob() -> Vec<Address> {
        vec![Address {
            name: None,
            email: "bob@example.org".into(),
        }]
    }

    fn message(bcc: bool) -> Vec<u8> {
        let mut builder = mail_builder::MessageBuilder::new()
            .from("alice@example.org")
            .to("bob@example.org")
            .subject("Visible subject")
            .text_body("Private message: café")
            .attachment("text/plain", "secret.txt", b"private attachment".to_vec());
        if bcc {
            builder = builder.bcc("carol@example.org");
        }
        builder.write_to_vec().unwrap()
    }

    fn policy(f: &Fixture, sign: bool, encrypt: bool) -> MailSecurity {
        MailSecurity {
            signing_fingerprint: f.alice.clone(),
            sign_by_default: sign,
            require_encryption: encrypt,
            smime: true,
            recipient_keys: BTreeMap::from([("bob@example.org".into(), f.bob.clone())]),
            ..Default::default()
        }
    }

    #[test]
    fn detects_smime_roots_only() {
        assert!(is_smime(
            b"Content-Type: application/pkcs7-mime; smime-type=enveloped-data\r\n\r\nAAAA\r\n"
        ));
        assert!(is_smime(b"Content-Type: multipart/signed; protocol=\"application/x-pkcs7-signature\"; boundary=b\r\n\r\n--b\r\n\r\nx\r\n--b--\r\n"));
        assert!(!is_smime(b"Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=b\r\n\r\n--b\r\n\r\nx\r\n--b--\r\n"));
        assert!(!is_smime(b"Content-Type: text/plain\r\n\r\nhello\r\n"));
    }

    #[test]
    fn listing_needs_valid_chain_identity_usage_and_exact_fingerprint() {
        let fpr = "A".repeat(40);
        let listing = format!(
            "crt:f:2048:1:KEYID:20260101T000000:20270101T000000:SERIAL::CN=Alice::esES::::::23:\nfpr:::::::::{fpr}:::ROOT:\nuid:f::::::::<alice@example.org>::\n"
        );
        assert!(validate_listing(&listing, &fpr, "Alice@Example.org", 's').is_ok());
        assert!(validate_listing(&listing, &fpr, "mallory@example.org", 's').is_err());
        assert!(validate_listing(&listing, &"B".repeat(40), "alice@example.org", 's').is_err());
        assert!(
            validate_listing(
                &listing.replace("crt:f", "crt:e"),
                &fpr,
                "alice@example.org",
                's'
            )
            .is_err()
        );
        assert!(
            validate_listing(
                &listing.replace("crt:f", "crt:-"),
                &fpr,
                "alice@example.org",
                's'
            )
            .is_err()
        );
        assert!(
            validate_listing(
                &listing.replace("esES", "s"),
                &fpr,
                "alice@example.org",
                'e'
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sign_and_encrypt_roundtrip_and_tampering() {
        let Some(f) = fixture().await else { return };
        let raw = message(false);
        let signed = f
            .smime
            .protect(&raw, &policy(&f, true, false), "alice@example.org", &bob())
            .await
            .unwrap();
        let wire = String::from_utf8_lossy(&signed);
        assert!(wire.contains("multipart/signed") && wire.contains("application/pkcs7-signature"));
        assert!(wire.contains("Visible subject") && wire.contains("micalg=\"sha-256\""));
        let opened = f.smime.open_message(&signed, Some(&f.alice)).await.unwrap();
        assert!(opened.text.contains("café"));
        assert!(
            opened.status.contains("verified certificate"),
            "{}",
            opened.status
        );
        assert!(opened.status.contains("Not encrypted"));
        let tampered = wire.replacen("Private message", "Private massage", 1);
        assert!(
            f.smime
                .open_message(tampered.as_bytes(), None)
                .await
                .is_err()
        );

        let both = f
            .smime
            .protect(&raw, &policy(&f, true, true), "alice@example.org", &bob())
            .await
            .unwrap();
        let wire = String::from_utf8_lossy(&both);
        assert!(wire.contains("smime-type=enveloped-data") && wire.contains("Visible subject"));
        assert!(!wire.contains("Private message") && !wire.contains("secret.txt"));
        let opened = f.smime.open_message(&both, None).await.unwrap();
        assert!(opened.text.contains("café"));
        assert!(
            opened.status.contains("Decrypted. Valid signature"),
            "{}",
            opened.status
        );
        assert!(opened.status.contains("1 attachment"));

        let sealed = f
            .smime
            .protect(&raw, &policy(&f, false, true), "alice@example.org", &bob())
            .await
            .unwrap();
        let opened = f.smime.open_message(&sealed, None).await.unwrap();
        assert!(
            opened.status.contains("does not prove"),
            "{}",
            opened.status
        );
        // Garbage ciphertext never yields text.
        let broken = String::from_utf8_lossy(&sealed).replacen("MI", "Mj", 2);
        assert!(f.smime.open_message(broken.as_bytes(), None).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_unsafe_or_unbound_encryption() {
        let Some(f) = fixture().await else { return };
        let encrypt = policy(&f, false, true);
        let err = f
            .smime
            .protect(&message(true), &encrypt, "alice@example.org", &bob())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Bcc"), "{err}");
        let stranger = vec![Address {
            name: None,
            email: "dave@example.org".into(),
        }];
        assert!(
            f.smime
                .preflight(&encrypt, "alice@example.org", &stranger)
                .await
                .is_err()
        );
        // Wrong identity, and a certificate from a root GnuPG does not trust.
        assert!(
            f.smime
                .preflight(&policy(&f, true, false), "bob@example.org", &[])
                .await
                .is_err()
        );
        let rogue = MailSecurity {
            signing_fingerprint: f.mallory.clone(),
            sign_by_default: true,
            smime: true,
            ..Default::default()
        };
        assert!(
            f.smime
                .preflight(&rogue, "mallory@example.org", &[])
                .await
                .is_err()
        );
        assert!(
            f.smime
                .preflight(&policy(&f, true, true), "alice@example.org", &bob())
                .await
                .is_ok()
        );
    }
}
