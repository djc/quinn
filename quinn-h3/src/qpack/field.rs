use std::borrow::Cow;
use std::fmt::{Display, Formatter};

/**
 * https://tools.ietf.org/html/rfc7541
 * 4.1.  Calculating Table Size
 */
pub const ESTIMATED_OVERHEAD_BYTES: usize = 32;

#[derive(Debug, PartialEq, Clone, Hash, Eq)]
pub struct HeaderField {
    pub name: Cow<'static, [u8]>,
    pub value: Cow<'static, [u8]>,
}

impl HeaderField {
    pub fn new<T, S>(name: T, value: S) -> HeaderField
    where
        T: Into<Vec<u8>>,
        S: Into<Vec<u8>>,
    {
        HeaderField {
            name: Cow::Owned(name.into()),
            value: Cow::Owned(value.into()),
        }
    }

    pub fn mem_size(&self) -> usize {
        self.name.len() + self.value.len() + ESTIMATED_OVERHEAD_BYTES
    }

    pub fn with_value<T>(&self, value: T) -> Self
    where
        T: Into<Vec<u8>>,
    {
        Self {
            name: self.name.to_owned(),
            value: Cow::Owned(value.into()),
        }
    }
}

impl Display for HeaderField {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "{}\t{}",
            String::from_utf8_lossy(&self.name),
            String::from_utf8_lossy(&self.value)
        )?;
        Ok(())
    }
}

impl From<HeaderField> for String {
    fn from(field: HeaderField) -> String {
        format!(
            "{}\t{}",
            String::from_utf8_lossy(&field.name),
            String::from_utf8_lossy(&field.value)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /**
     * https://tools.ietf.org/html/rfc7541#section-4.1
     * "The size of an entry is the sum of its name's length in octets (as
     *  defined in Section 5.2), its value's length in octets, and 32."
     * "The size of an entry is calculated using the length of its name and
     *  value without any Huffman encoding applied."
     */
    #[test]
    fn test_field_size_is_offset_by_32() {
        let field = HeaderField {
            name: Cow::Borrowed(b"Name"),
            value: Cow::Borrowed(b"Value"),
        };
        assert_eq!(field.mem_size(), 4 + 5 + 32);
    }

    #[test]
    fn with_value() {
        let field = HeaderField {
            name: Cow::Borrowed(b"Name"),
            value: Cow::Borrowed(b"Value"),
        };
        assert_eq!(
            field.with_value("New value"),
            HeaderField {
                name: Cow::Borrowed(b"Name"),
                value: Cow::Borrowed(b"New value"),
            }
        );
    }
}
