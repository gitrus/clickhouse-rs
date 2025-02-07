use std::{fmt, ops, sync::Arc, marker};

use chrono_tz::Tz;

use crate::{
    binary::{Encoder, ReadEx},
    errors::{Error, FromSqlError, Result},
    types::{
        column::{
            column_data::ArcColumnData,
            decimal::{DecimalAdapter, NullableDecimalAdapter},
            fixed_string::{FixedStringAdapter, NullableFixedStringAdapter},
            string::StringAdapter,
            iter::SimpleIterable,
        },
        decimal::NoBits,
        SqlType, Value, ValueRef,
    },
};

use self::chunk::ChunkColumnData;
pub(crate) use self::string_pool::StringPool;
pub use self::{column_data::ColumnData, concat::ConcatColumnData, numeric::VectorColumnData};

mod array;
mod chunk;
mod column_data;
mod concat;
mod date;
mod decimal;
mod factory;
pub(crate) mod fixed_string;
mod iter;
mod list;
mod nullable;
mod numeric;
mod string;
mod string_pool;

/// Represents Clickhouse Column
pub struct Column<K: ColumnType> {
    pub(crate) name: String,
    pub(crate) data: ArcColumnData,
    pub(crate) _marker: marker::PhantomData<K>,
}

pub trait ColumnFrom {
    fn column_from<W: ColumnWrapper>(source: Self) -> W::Wrapper;
}

pub trait ColumnType: Send + Copy + Sync + 'static {}

#[derive(Copy, Clone)]
pub struct Simple {
    _private: (),
}

#[derive(Copy, Clone)]
pub struct Complex {
    _private: (),
}

impl ColumnType for Simple {}

impl ColumnType for Complex {}

impl Default for Simple {
    fn default() -> Self {
        Self { _private: () }
    }
}

impl Default for Complex {
    fn default() -> Self {
        Self { _private: () }
    }
}

impl<K: ColumnType> ColumnFrom for Column<K> {
    fn column_from<W: ColumnWrapper>(source: Self) -> W::Wrapper {
        W::wrap_arc(source.data)
    }
}

impl<L: ColumnType, R: ColumnType> PartialEq<Column<R>> for Column<L> {
    fn eq(&self, other: &Column<R>) -> bool {
        if self.len() != other.len() {
            return false;
        }

        if self.sql_type() != other.sql_type() {
            return false;
        }

        for i in 0..self.len() {
            if self.at(i) != other.at(i) {
                return false;
            }
        }

        true
    }
}

impl<K: ColumnType> Clone for Column<K> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            data: self.data.clone(),
            _marker: marker::PhantomData,
        }
    }
}

impl Column<Simple> {
    pub(crate) fn concat<'a, I>(items: I) -> Column<Complex>
    where
        I: Iterator<Item = &'a Self>,
    {
        let items_vec: Vec<&Self> = items.collect();
        let chunks: Vec<_> = items_vec.iter().map(|column| column.data.clone()).collect();
        match items_vec.first() {
            None => unreachable!(),
            Some(ref first_column) => {
                let name: String = first_column.name().to_string();
                let data = ConcatColumnData::concat(chunks);
                Column {
                    name,
                    data: Arc::new(data),
                    _marker: marker::PhantomData,
                }
            }
        }
    }

    /// Returns an iterator over the column.
    ///
    /// ### Example
    ///
    /// ```rust
    /// # extern crate clickhouse_rs;
    /// # extern crate futures;
    /// # use clickhouse_rs::Pool;
    /// # use futures::Future;
    /// # use std::env;
    /// # let database_url =
    /// #     env::var("DATABASE_URL").unwrap_or("tcp://localhost:9000?compression=lz4".into());
    ///   let pool = Pool::new(database_url);
    ///   let done = pool
    ///       .get_handle()
    ///       .and_then(|c| {
    ///           let sql_query = "SELECT number as n1, number as n2, number as n3 FROM numbers(100)";
    ///           c.query(sql_query).fold_blocks(0_u64, |mut sum, block| {
    ///
    ///               let c1 = block.get_column("n1")?.iter::<u64>()?;
    ///               let c2 = block.get_column("n2")?.iter::<u64>()?;
    ///               let c3 = block.get_column("n3")?.iter::<u64>()?;
    ///
    ///               for ((v1, v2), v3) in c1.zip(c2).zip(c3) {
    ///                   sum += v1 + v2 + v3;
    ///               }
    ///
    ///               Ok(sum)
    ///           })
    ///       })
    ///       .map(|(_, sum)| { dbg!(sum); })
    ///       .map_err(|err| eprintln!("database error: {}", err));
    /// # tokio::run(done)
    /// ```
    pub fn iter<'a, T>(&'a self) -> Result<T::Iter>
    where
        T: SimpleIterable<'a>,
    {
        T::iter(self, self.sql_type())
    }
}

impl<K: ColumnType> Column<K> {
    pub(crate) fn read<R: ReadEx>(reader: &mut R, size: usize, tz: Tz) -> Result<Column<K>> {
        let name = reader.read_string()?;
        let type_name = reader.read_string()?;
        let data = ColumnData::load_data::<ArcColumnWrapper, _>(reader, &type_name, size, tz)?;
        let column = Self {
            name,
            data,
            _marker: marker::PhantomData,
        };
        Ok(column)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn sql_type(&self) -> SqlType {
        self.data.sql_type()
    }

    pub(crate) fn at(&self, index: usize) -> ValueRef {
        self.data.at(index)
    }

    pub(crate) fn write(&self, encoder: &mut Encoder) {
        encoder.string(&self.name);
        encoder.string(self.data.sql_type().to_string().as_ref());
        let len = self.data.len();
        self.data.save(encoder, 0, len);
    }

    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    pub(crate) fn slice(&self, range: ops::Range<usize>) -> Column<Complex> {
        let data = ChunkColumnData::new(self.data.clone(), range);
        Column {
            name: self.name.clone(),
            data: Arc::new(data),
            _marker: marker::PhantomData,
        }
    }

    pub(crate) fn cast_to(self, dst_type: SqlType) -> Result<Self> {
        let src_type = self.sql_type();

        if dst_type == src_type {
            return Ok(self);
        }

        match (dst_type, src_type) {
            (SqlType::FixedString(str_len), SqlType::String) => {
                let name = self.name().to_owned();
                let adapter = FixedStringAdapter {
                    column: self,
                    str_len,
                };
                Ok(Column {
                    name,
                    data: Arc::new(adapter),
                    _marker: marker::PhantomData,
                })
            }
            (
                SqlType::Nullable(SqlType::FixedString(str_len)),
                SqlType::Nullable(SqlType::String),
            ) => {
                let name = self.name().to_owned();
                let adapter = NullableFixedStringAdapter {
                    column: self,
                    str_len: *str_len,
                };
                Ok(Column {
                    name,
                    data: Arc::new(adapter),
                    _marker: marker::PhantomData,
                })
            }
            (SqlType::String, SqlType::Array(SqlType::UInt8)) => {
                let name = self.name().to_owned();
                let adapter = StringAdapter { column: self };
                Ok(Column {
                    name,
                    data: Arc::new(adapter),
                    _marker: marker::PhantomData,
                })
            }
            (SqlType::FixedString(n), SqlType::Array(SqlType::UInt8)) => {
                let string_column = self.cast_to(SqlType::String)?;
                string_column.cast_to(SqlType::FixedString(n))
            }
            (SqlType::Decimal(dst_p, dst_s), SqlType::Decimal(_, _)) => {
                let name = self.name().to_owned();
                let nobits = NoBits::from_precision(dst_p).unwrap();
                let adapter = DecimalAdapter {
                    column: self,
                    precision: dst_p,
                    scale: dst_s,
                    nobits,
                };
                Ok(Column {
                    name,
                    data: Arc::new(adapter),
                    _marker: marker::PhantomData,
                })
            }
            (
                SqlType::Nullable(SqlType::Decimal(dst_p, dst_s)),
                SqlType::Nullable(SqlType::Decimal(_, _)),
            ) => {
                let name = self.name().to_owned();
                let nobits = NoBits::from_precision(*dst_p).unwrap();
                let adapter = NullableDecimalAdapter {
                    column: self,
                    precision: *dst_p,
                    scale: *dst_s,
                    nobits,
                };
                Ok(Column {
                    name,
                    data: Arc::new(adapter),
                    _marker: marker::PhantomData,
                })
            }
            _ => Err(Error::FromSql(FromSqlError::InvalidType {
                src: src_type.to_string(),
                dst: dst_type.to_string(),
            })),
        }
    }

    pub(crate) fn push(&mut self, value: Value) {
        loop {
            match Arc::get_mut(&mut self.data) {
                None => {
                    self.data = Arc::from(self.data.clone_instance());
                }
                Some(data) => {
                    data.push(value);
                    break;
                }
            }
        }
    }

    pub(crate) unsafe fn get_internal(&self, pointers: &[*mut *const u8], level: u8) -> Result<()> {
        self.data.get_internal(pointers, level)
    }
}

pub(crate) fn new_column<K: ColumnType>(
    name: &str,
    data: Arc<(dyn ColumnData + Sync + Send + 'static)>,
) -> Column<K> {
    Column {
        name: name.to_string(),
        data,
        _marker: marker::PhantomData,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Either<L, R>
where
    L: fmt::Debug + PartialEq + Clone,
    R: fmt::Debug + PartialEq + Clone,
{
    Left(L),
    Right(R),
}

pub trait ColumnWrapper {
    type Wrapper;
    fn wrap<T: ColumnData + Send + Sync + 'static>(column: T) -> Self::Wrapper;

    fn wrap_arc(data: ArcColumnData) -> Self::Wrapper;
}

pub(crate) struct ArcColumnWrapper {}

impl ColumnWrapper for ArcColumnWrapper {
    type Wrapper = Arc<dyn ColumnData + Send + Sync>;

    fn wrap<T: ColumnData + Send + Sync + 'static>(column: T) -> Self::Wrapper {
        Arc::new(column)
    }

    fn wrap_arc(data: ArcColumnData) -> Self::Wrapper {
        data
    }
}

pub(crate) struct BoxColumnWrapper {}

impl ColumnWrapper for BoxColumnWrapper {
    type Wrapper = Box<dyn ColumnData + Send + Sync>;

    fn wrap<T: ColumnData + Send + Sync + 'static>(column: T) -> Self::Wrapper {
        Box::new(column)
    }

    fn wrap_arc(_: ArcColumnData) -> Self::Wrapper {
        unimplemented!()
    }
}
