use serde::{Deserialize, Serialize};

use crate::Oid;
use crate::models::CommitSignature;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagAnnotation {
    pub message: String,
    pub tagger: Option<CommitSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    /// The commit the tag ultimately points at.
    pub target: Oid,
    /// `None` for lightweight tags.
    pub annotation: Option<TagAnnotation>,
}

impl Tag {
    /// Full ref name, `refs/tags/<name>`.
    pub fn refname(&self) -> String {
        format!("refs/tags/{}", self.name)
    }

    pub fn is_annotated(&self) -> bool {
        self.annotation.is_some()
    }

    /// The annotation message, or `""` for lightweight tags.
    pub fn message(&self) -> &str {
        self.annotation.as_ref().map_or("", |a| a.message.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{oid, round_trip};

    fn annotated() -> Tag {
        Tag {
            name: "v1.0".into(),
            target: oid(4),
            annotation: Some(TagAnnotation {
                message: "Release 1.0\n\nNotes".into(),
                tagger: Some(CommitSignature {
                    name: "Ada".into(),
                    email: "ada@example.com".into(),
                }),
            }),
        }
    }

    fn lightweight() -> Tag {
        Tag {
            name: "nightly".into(),
            target: oid(5),
            annotation: None,
        }
    }

    #[test]
    fn accessors() {
        assert_eq!(annotated().refname(), "refs/tags/v1.0");
        assert!(annotated().is_annotated() && !lightweight().is_annotated());
        assert_eq!(lightweight().message(), "");
        assert!(annotated().message().starts_with("Release"));
    }

    #[test]
    fn serde_round_trip() {
        round_trip(&annotated());
        round_trip(&lightweight());
        // Annotated tag without a tagger (allowed by git; the old model invented an "Unknown" signature).
        round_trip(&Tag {
            annotation: Some(TagAnnotation {
                message: String::new(),
                tagger: None,
            }),
            ..annotated()
        });
    }
}
