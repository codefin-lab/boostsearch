//! SQL and PPL: two languages over the same index.
//!
//! Neither is a second engine. A query in either is read into the same shape
//! -- which documents, grouped how, showing what -- and that shape becomes an
//! ordinary search: a query, some aggregations, a sort and a size. What comes
//! back is turned into rows and columns, because that is what somebody who
//! wrote SQL is expecting to be handed.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod plan;
pub mod ppl;
pub mod rows;
