use bytes::{Buf, BufMut};
use std::cmp;
use std::io::Cursor;

use super::bloc::{
    HeaderPrefix, Indexed, IndexedWithPostBase, Literal, LiteralWithNameRef,
    LiteralWithPostBaseNameRef,
};
use super::prefix_int::Error as IntError;
use super::prefix_string::Error as StringError;
use super::stream::{
    DecoderInstruction, Duplicate, HeaderAck, InsertCountIncrement, InsertWithNameRef,
    InsertWithoutNameRef, StreamCancel,
};
use super::table::{
    DynamicInsertionResult, DynamicLookupResult, DynamicTable, DynamicTableEncoder,
    DynamicTableError, HeaderField, StaticTable,
};
use super::ParseError;

pub fn encode<W: BufMut>(
    table: &mut DynamicTableEncoder,
    bloc: &mut W,
    encoder: &mut W,
    fields: &[HeaderField],
) -> Result<usize, Error> {
    let mut required_ref = 0;
    let mut bloc_buf = Vec::new();

    for field in fields {
        if let Some(reference) = encode_field(table, &mut bloc_buf, encoder, field)? {
            required_ref = cmp::max(required_ref, reference);
        }
    }

    HeaderPrefix::new(
        required_ref,
        table.base(),
        table.total_inserted(),
        table.max_mem_size(),
    )
    .encode(bloc);
    bloc.put(bloc_buf);

    table.commit();

    Ok(required_ref)
}

fn encode_field<W: BufMut>(
    table: &mut DynamicTableEncoder,
    bloc: &mut Vec<u8>,
    encoder: &mut W,
    field: &HeaderField,
) -> Result<Option<usize>, Error> {
    if let Some(index) = StaticTable::find(field) {
        Indexed::Static(index).encode(bloc);
        return Ok(None);
    }

    if let DynamicLookupResult::Relative { index, absolute } = table.find(field) {
        Indexed::Dynamic(index).encode(bloc);
        return Ok(Some(absolute));
    }

    let reference = match table.insert(field)? {
        DynamicInsertionResult::Duplicated {
            relative,
            postbase,
            absolute,
        } => {
            Duplicate(relative).encode(encoder);
            IndexedWithPostBase(postbase).encode(bloc);
            Some(absolute)
        }
        DynamicInsertionResult::Inserted { postbase, absolute } => {
            InsertWithoutNameRef::new(field.name.clone(), field.value.clone()).encode(encoder)?;
            IndexedWithPostBase(postbase).encode(bloc);
            Some(absolute)
        }
        DynamicInsertionResult::InsertedWithStaticNameRef {
            postbase,
            index,
            absolute,
        } => {
            InsertWithNameRef::new_static(index, field.value.clone()).encode(encoder)?;
            IndexedWithPostBase(postbase).encode(bloc);
            Some(absolute)
        }
        DynamicInsertionResult::InsertedWithNameRef {
            postbase,
            relative,
            absolute,
        } => {
            InsertWithNameRef::new_dynamic(relative, field.value.clone()).encode(encoder)?;
            IndexedWithPostBase(postbase).encode(bloc);
            Some(absolute)
        }
        DynamicInsertionResult::NotInserted(lookup_result) => match lookup_result {
            DynamicLookupResult::Static(index) => {
                LiteralWithNameRef::new_static(index, field.value.clone()).encode(bloc)?;
                None
            }
            DynamicLookupResult::Relative { index, absolute } => {
                LiteralWithNameRef::new_dynamic(index, field.value.clone()).encode(bloc)?;
                Some(absolute)
            }
            DynamicLookupResult::PostBase { index, absolute } => {
                LiteralWithPostBaseNameRef::new(index, field.value.clone()).encode(bloc)?;
                Some(absolute)
            }
            DynamicLookupResult::NotFound => {
                Literal::new(field.name.clone(), field.value.clone()).encode(bloc)?;
                None
            }
        },
    };
    Ok(reference)
}

pub fn on_decoder_recv<R: Buf>(table: &mut DynamicTable, read: &mut R) -> Result<(), Error> {
    while let Some(instruction) = parse_instruction(read)? {
        match instruction {
            Instruction::Untrack(stream_id) => table.untrack_bloc(stream_id)?,
            Instruction::RecievedRef(idx) => table.update_largest_recieved(idx),
        }
    }
    Ok(())
}

fn parse_instruction<R: Buf>(read: &mut R) -> Result<Option<Instruction>, Error> {
    if read.remaining() < 1 {
        return Ok(None);
    }

    let mut buf = Cursor::new(read.bytes());
    let first = buf.bytes()[0];
    let instruction = match DecoderInstruction::decode(first) {
        DecoderInstruction::Unknown => return Err(Error::UnknownPrefix),
        DecoderInstruction::InsertCountIncrement => {
            InsertCountIncrement::decode(&mut buf)?.map(|x| Instruction::RecievedRef(x.0))
        }
        DecoderInstruction::HeaderAck => {
            HeaderAck::decode(&mut buf)?.map(|x| Instruction::Untrack(x.0))
        }
        DecoderInstruction::StreamCancel => {
            StreamCancel::decode(&mut buf)?.map(|x| Instruction::Untrack(x.0))
        }
    };

    if instruction.is_some() {
        read.advance(buf.position() as usize);
    }

    Ok(instruction)
}

#[derive(Debug, PartialEq)]
enum Instruction {
    RecievedRef(usize),
    Untrack(u64),
}

#[derive(Debug, PartialEq)]
pub enum Error {
    Insertion(DynamicTableError),
    InvalidString(StringError),
    InvalidInteger(IntError),
    UnknownPrefix,
}

impl From<DynamicTableError> for Error {
    fn from(e: DynamicTableError) -> Self {
        Error::Insertion(e)
    }
}

impl From<StringError> for Error {
    fn from(e: StringError) -> Self {
        Error::InvalidString(e)
    }
}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        match e {
            ParseError::InvalidInteger(x) => Error::InvalidInteger(x),
            ParseError::InvalidString(x) => Error::InvalidString(x),
            ParseError::InvalidPrefix(_) => Error::UnknownPrefix,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qpack::table::SETTINGS_HEADER_TABLE_SIZE_DEFAULT as TABLE_SIZE;

    fn check_encode_field(
        init_fields: &[HeaderField],
        field: &[HeaderField],
        check: &Fn(&mut Cursor<&mut Vec<u8>>, &mut Cursor<&mut Vec<u8>>),
    ) {
        check_encode_field_table(&mut DynamicTable::new(), init_fields, field, 1, check);
    }

    fn check_encode_field_table(
        table: &mut DynamicTable,
        init_fields: &[HeaderField],
        field: &[HeaderField],
        stream_id: u64,
        check: &Fn(&mut Cursor<&mut Vec<u8>>, &mut Cursor<&mut Vec<u8>>),
    ) {
        for field in init_fields {
            table.inserter().put_field(field.clone()).unwrap();
        }

        let mut encoder = Vec::new();
        let mut bloc = Vec::new();
        let mut enc_table = table.encoder(stream_id);

        for field in field {
            encode_field(&mut enc_table, &mut bloc, &mut encoder, field).unwrap();
        }

        enc_table.commit();

        let mut read_bloc = Cursor::new(&mut bloc);
        let mut read_encoder = Cursor::new(&mut encoder);
        check(&mut read_bloc, &mut read_encoder);
    }

    #[test]
    fn encode_static() {
        let field = HeaderField::new(":method", "GET");
        check_encode_field(&[], &[field], &|mut b, e| {
            assert_eq!(Indexed::decode(&mut b), Ok(Indexed::Static(17)));
            assert_eq!(e.get_ref().len(), 0);
        });
    }

    #[test]
    fn encode_static_nameref() {
        let field = HeaderField::new("location", "/bar");
        check_encode_field(&[], &[field], &|mut b, mut e| {
            assert_eq!(
                IndexedWithPostBase::decode(&mut b),
                Ok(IndexedWithPostBase(0))
            );
            assert_eq!(
                InsertWithNameRef::decode(&mut e),
                Ok(Some(InsertWithNameRef::new_static(12, "/bar")))
            );
        });
    }

    #[test]
    fn encode_static_nameref_indexed_in_dynamic() {
        let field = HeaderField::new("location", "/bar");
        check_encode_field(&[field.clone()], &[field], &|mut b, e| {
            assert_eq!(Indexed::decode(&mut b), Ok(Indexed::Dynamic(0)));
            assert_eq!(e.get_ref().len(), 0);
        });
    }

    #[test]
    fn encode_dynamic_insert() {
        let field = HeaderField::new("foo", "bar");
        check_encode_field(&[], &[field], &|mut b, mut e| {
            assert_eq!(
                IndexedWithPostBase::decode(&mut b),
                Ok(IndexedWithPostBase(0))
            );
            assert_eq!(
                InsertWithoutNameRef::decode(&mut e),
                Ok(Some(InsertWithoutNameRef::new("foo", "bar")))
            );
        });
    }

    #[test]
    fn encode_dynamic_insert_nameref() {
        let field = HeaderField::new("foo", "bar");
        check_encode_field(
            &[field.clone(), HeaderField::new("baz", "bar")],
            &[field.with_value("quxx")],
            &|mut b, mut e| {
                assert_eq!(
                    IndexedWithPostBase::decode(&mut b),
                    Ok(IndexedWithPostBase(0))
                );
                assert_eq!(
                    InsertWithNameRef::decode(&mut e),
                    Ok(Some(InsertWithNameRef::new_dynamic(1, "quxx")))
                );
            },
        );
    }

    #[test]
    fn encode_literal() {
        let mut table = DynamicTable::new();
        table.inserter().set_max_mem_size(0).unwrap();
        let field = HeaderField::new("foo", "bar");
        check_encode_field_table(&mut table, &[], &[field], 1, &|mut b, e| {
            assert_eq!(Literal::decode(&mut b), Ok(Literal::new("foo", "bar")));
            assert_eq!(e.get_ref().len(), 0);
        });
    }

    #[test]
    fn encode_literal_nameref() {
        let mut table = DynamicTable::new();
        table.inserter().set_max_mem_size(63).unwrap();
        let field = HeaderField::new("foo", "bar");

        check_encode_field_table(&mut table, &[], &[field.clone()], 1, &|mut b, _| {
            assert_eq!(
                IndexedWithPostBase::decode(&mut b),
                Ok(IndexedWithPostBase(0))
            );
        });
        check_encode_field_table(
            &mut table,
            &[field.clone()],
            &[field.with_value("quxx")],
            2,
            &|mut b, e| {
                assert_eq!(
                    LiteralWithNameRef::decode(&mut b),
                    Ok(LiteralWithNameRef::new_dynamic(0, "quxx"))
                );
                assert_eq!(e.get_ref().len(), 0);
            },
        );
    }

    #[test]
    fn encode_literal_postbase_nameref() {
        let mut table = DynamicTable::new();
        table.inserter().set_max_mem_size(63).unwrap();
        let field = HeaderField::new("foo", "bar");
        check_encode_field_table(
            &mut table,
            &[],
            &[field.clone(), field.with_value("quxx")],
            1,
            &|mut b, mut e| {
                assert_eq!(
                    IndexedWithPostBase::decode(&mut b),
                    Ok(IndexedWithPostBase(0))
                );
                assert_eq!(
                    LiteralWithPostBaseNameRef::decode(&mut b),
                    Ok(LiteralWithPostBaseNameRef::new(0, "quxx"))
                );
                assert_eq!(
                    InsertWithoutNameRef::decode(&mut e),
                    Ok(Some(InsertWithoutNameRef::new("foo", "bar")))
                );
            },
        );
    }

    #[test]
    fn encode_with_header_bloc() {
        let mut table = DynamicTable::new();

        for idx in 1..5 {
            table
                .inserter()
                .put_field(HeaderField::new(
                    format!("foo{}", idx),
                    format!("bar{}", idx),
                ))
                .unwrap();
        }

        let mut encoder = Vec::new();
        let mut bloc = Vec::new();

        let fields = [
            HeaderField::new(":method", "GET"),
            HeaderField::new("foo1", "bar1"),
            HeaderField::new("foo3", "new bar3"),
            HeaderField::new(":method", "staticnameref"),
            HeaderField::new("newfoo", "newbar"),
        ];

        assert_eq!(
            encode(&mut table.encoder(1), &mut bloc, &mut encoder, &fields),
            Ok(7)
        );

        let mut read_bloc = Cursor::new(&mut bloc);
        let mut read_encoder = Cursor::new(&mut encoder);

        assert_eq!(
            InsertWithNameRef::decode(&mut read_encoder),
            Ok(Some(InsertWithNameRef::new_dynamic(1, "new bar3")))
        );
        assert_eq!(
            InsertWithNameRef::decode(&mut read_encoder),
            Ok(Some(InsertWithNameRef::new_static(
                StaticTable::find_name(&b":method"[..]).unwrap(),
                "staticnameref"
            )))
        );
        assert_eq!(
            InsertWithoutNameRef::decode(&mut read_encoder),
            Ok(Some(InsertWithoutNameRef::new("newfoo", "newbar")))
        );

        assert_eq!(
            HeaderPrefix::decode(&mut read_bloc)
                .unwrap()
                .get(7, TABLE_SIZE),
            Ok((7, 4))
        );
        assert_eq!(Indexed::decode(&mut read_bloc), Ok(Indexed::Static(17)));
        assert_eq!(Indexed::decode(&mut read_bloc), Ok(Indexed::Dynamic(3)));
        assert_eq!(
            IndexedWithPostBase::decode(&mut read_bloc),
            Ok(IndexedWithPostBase(0))
        );
        assert_eq!(
            IndexedWithPostBase::decode(&mut read_bloc),
            Ok(IndexedWithPostBase(1))
        );
        assert_eq!(
            IndexedWithPostBase::decode(&mut read_bloc),
            Ok(IndexedWithPostBase(2))
        );
        assert_eq!(read_bloc.get_ref().len() as u64, read_bloc.position());
    }

    #[test]
    fn decoder_bloc_ack() {
        let mut table = DynamicTable::new();

        let field = HeaderField::new("foo", "bar");
        check_encode_field_table(
            &mut table,
            &[],
            &[field.clone(), field.with_value("quxx")],
            2,
            &|_, _| {},
        );

        let mut buf = vec![];

        HeaderAck(2).encode(&mut buf);
        let mut cur = Cursor::new(&buf);
        assert_eq!(
            parse_instruction(&mut cur),
            Ok(Some(Instruction::Untrack(2)))
        );

        let mut cur = Cursor::new(&buf);
        assert_eq!(on_decoder_recv(&mut table, &mut cur), Ok(()),);

        let mut cur = Cursor::new(&buf);
        assert_eq!(
            on_decoder_recv(&mut table, &mut cur),
            Err(Error::Insertion(DynamicTableError::UnknownStreamId(2)))
        );
    }

    #[test]
    fn decoder_stream_cacnceled() {
        let mut table = DynamicTable::new();

        let field = HeaderField::new("foo", "bar");
        check_encode_field_table(
            &mut table,
            &[],
            &[field.clone(), field.with_value("quxx")],
            2,
            &|_, _| {},
        );

        let mut buf = vec![];

        StreamCancel(2).encode(&mut buf);
        let mut cur = Cursor::new(&buf);
        assert_eq!(
            parse_instruction(&mut cur),
            Ok(Some(Instruction::Untrack(2)))
        );
    }

    #[test]
    fn decoder_accept_trucated() {
        let mut buf = vec![];
        StreamCancel(2321).encode(&mut buf);

        let mut cur = Cursor::new(&buf[..2]); // trucated prefix_int
        assert_eq!(parse_instruction(&mut cur), Ok(None));

        let mut cur = Cursor::new(&buf);
        assert_eq!(
            parse_instruction(&mut cur),
            Ok(Some(Instruction::Untrack(2321)))
        );
    }

    #[test]
    fn decoder_unknown_stream() {
        let mut table = DynamicTable::new();

        check_encode_field_table(
            &mut table,
            &[],
            &[HeaderField::new("foo", "bar")],
            2,
            &|_, _| {},
        );

        let mut buf = vec![];
        StreamCancel(4).encode(&mut buf);

        let mut cur = Cursor::new(&buf);
        assert_eq!(
            on_decoder_recv(&mut table, &mut cur),
            Err(Error::Insertion(DynamicTableError::UnknownStreamId(4)))
        );
    }

    #[test]
    fn insert_count() {
        let mut buf = vec![];
        InsertCountIncrement(4).encode(&mut buf);

        let mut cur = Cursor::new(&buf);
        assert_eq!(
            parse_instruction(&mut cur),
            Ok(Some(Instruction::RecievedRef(4)))
        );

        let mut cur = Cursor::new(&buf);
        assert_eq!(on_decoder_recv(&mut DynamicTable::new(), &mut cur), Ok(()));
    }
}
