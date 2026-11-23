//! Validated identifiers at marketplace authority boundaries. The public wire spelling is unchanged.
use std::fmt;

/// Accepts the established Orange identifier alphabet, including pre-UUID development records.
pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidId;
impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid marketplace identifier")
    }
}
impl std::error::Error for InvalidId {}

macro_rules! id {
    ($name:ident,$description:literal) => {
        #[doc=$description]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name<'a>(&'a str);
        impl<'a> TryFrom<&'a str> for $name<'a> {
            type Error = InvalidId;
            fn try_from(value: &'a str) -> Result<Self, Self::Error> {
                if valid_id(value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidId)
                }
            }
        }
        impl<'a> $name<'a> {
            pub fn as_str(self) -> &'a str {
                self.0
            }
        }
    };
}

id!(
    OwnerId,
    "Account ownership identifier. A user identity must be resolved to its owner at the authority boundary."
);
id!(JobId, "Job identifier issued by the control plane.");
id!(
    UserId,
    "Authenticated application user, independent of the account responsible for a Job."
);

id!(
    OrganizationId,
    "Organization identifier used by membership and resource ACLs."
);
id!(
    NodeId,
    "Registered supplier device identity, authenticated by the control connection."
);
id!(
    ModelVersionId,
    "Version in the platform's reviewed execution profile catalogue."
);
id!(
    AttemptId,
    "Specific execution attempt of a Job, distinct from its lease and node."
);
id!(
    ArtifactId,
    "Owner-scoped artifact identity, independent of a particular version."
);
id!(
    ArtifactVersionId,
    "Exact artifact content version bound to an operation."
);
id!(
    LedgerTransactionId,
    "An immutable posting group in the account ledger."
);

#[derive(Clone, Copy)]
pub struct AssignedExecution<'a> {
    pub job: JobId<'a>,
    pub attempt: AttemptId<'a>,
    pub node: NodeId<'a>,
    pub model: ModelVersionId<'a>,
}

/// Organization membership always binds a user to an organization, never to a supplier node.
///
/// ```compile_fail
/// use orange_domain::identity::{Membership,NodeId,UserId};
/// let _ = Membership { organization:NodeId::try_from("device").unwrap(), user:UserId::try_from("user").unwrap() };
/// ```
pub struct Membership<'a> {
    pub organization: OrganizationId<'a>,
    pub user: UserId<'a>,
}

/// Typed coordinates for a submitted Job. The authenticated user is used in addition
/// to the owner in user-facing lookups.
///
/// ```compile_fail
/// use orange_domain::identity::{JobId,OwnerId,SubmittedJob,UserId};
/// let owner=OwnerId::try_from("organization:1").unwrap();
/// let user=UserId::try_from("user:1").unwrap();
/// let job=JobId::try_from("job:1").unwrap();
/// let _ = SubmittedJob { owner:user, submitter:owner, job };
/// ```
pub struct SubmittedJob<'a> {
    pub owner: OwnerId<'a>,
    pub submitter: UserId<'a>,
    pub job: JobId<'a>,
}

/// A version key without a verified content digest is not a pinned artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactPin<'a> {
    artifact: ArtifactId<'a>,
    version: ArtifactVersionId<'a>,
    digest: &'a str,
}
pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl<'a> ArtifactPin<'a> {
    pub fn new(artifact: &'a str, version: &'a str, sha256: &'a str) -> Result<Self, InvalidId> {
        if !valid_digest(sha256) {
            return Err(InvalidId);
        }
        Ok(Self {
            artifact: ArtifactId::try_from(artifact)?,
            version: ArtifactVersionId::try_from(version)?,
            digest: sha256,
        })
    }
    pub fn artifact(self) -> ArtifactId<'a> {
        self.artifact
    }
    pub fn version(self) -> ArtifactVersionId<'a> {
        self.version
    }
    pub fn digest(self) -> &'a str {
        self.digest
    }
}

/// Nonnegative millisecond counts use the project's canonical decimal-string wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Milliseconds(u64);
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidMilliseconds;
impl fmt::Display for InvalidMilliseconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid millisecond count")
    }
}
impl std::error::Error for InvalidMilliseconds {}
impl std::str::FromStr for Milliseconds {
    type Err = InvalidMilliseconds;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty()
            || (text.len() > 1 && text.starts_with('0'))
            || !text.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(InvalidMilliseconds);
        }
        text.parse::<u64>()
            .map(Self)
            .map_err(|_| InvalidMilliseconds)
    }
}
impl Milliseconds {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_preserve_valid_legacy_spelling_and_reject_bad_boundaries() {
        for id in ["old_node:az-1.02", "123", "A:B.C_D-99"] {
            assert_eq!(OwnerId::try_from(id).expect("valid legacy ID").as_str(), id);
            assert_eq!(JobId::try_from(id).expect("valid legacy ID").as_str(), id);
            assert_eq!(UserId::try_from(id).expect("valid legacy ID").as_str(), id);
        }
        assert!(JobId::try_from("x".repeat(128).as_str()).is_ok());
        for id in [
            "",
            "x/y",
            "owner name",
            "\n",
            "用户",
            "x".repeat(129).as_str(),
        ] {
            assert_eq!(OwnerId::try_from(id), Err(InvalidId));
        }
    }
}
