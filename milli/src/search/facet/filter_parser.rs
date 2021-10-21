//! BNF grammar:
//!
//! ```text
//! expression     = or
//! or             = and (~ "OR" ~ and)
//! and            = not (~ "AND" not)*
//! not            = ("NOT" | "!") not | primary
//! primary        = (WS* ~ "("  expression ")" ~ WS*) | condition | to | geoRadius
//! to             = value value TO value
//! condition      = value ("==" | ">" ...) value
//! value          = WS* ~ ( word | singleQuoted | doubleQuoted) ~ WS*
//! singleQuoted   = "'" .* all but quotes "'"
//! doubleQuoted   = "\"" (word | spaces)* "\""
//! word           = (alphanumeric | _ | - | .)+
//! geoRadius      = WS* ~ "_geoRadius(float ~ "," ~ float ~ "," float)
//! ```

use std::collections::HashSet;
use std::fmt::Debug;
use std::result::Result as StdResult;

use nom::branch::alt;
use nom::bytes::complete::{tag, take_till, take_while1};
use nom::character::complete::{char, multispace0};
use nom::combinator::map;
use nom::error::{ContextError, ErrorKind, VerboseError};
use nom::multi::{many0, separated_list1};
use nom::number::complete::recognize_float;
use nom::sequence::{delimited, preceded, tuple};
use nom::IResult;
use nom_locate::LocatedSpan;

use self::Operator::*;
use super::FilterCondition;
use crate::{FieldId, FieldsIdsMap};

pub enum FilterError {
    AttributeNotFilterable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token<'a> {
    pub position: Span<'a>,
    pub inner: &'a str,
}

type Span<'a> = LocatedSpan<&'a str>;

#[derive(Debug, Clone)]
pub enum Operator<'a> {
    GreaterThan(Token<'a>),
    GreaterThanOrEqual(Token<'a>),
    Equal(Option<Token<'a>>, Token<'a>),
    NotEqual(Option<Token<'a>>, Token<'a>),
    LowerThan(Token<'a>),
    LowerThanOrEqual(Token<'a>),
    Between(Token<'a>, Token<'a>),
    GeoLowerThan([Token<'a>; 2], Token<'a>),
    GeoGreaterThan([Token<'a>; 2], Token<'a>),
}

impl<'a> Operator<'a> {
    /// This method can return two operations in case it must express
    /// an OR operation for the between case (i.e. `TO`).
    pub fn negate(self) -> (Self, Option<Self>) {
        match self {
            GreaterThan(n) => (LowerThanOrEqual(n), None),
            GreaterThanOrEqual(n) => (LowerThan(n), None),
            Equal(n, s) => (NotEqual(n, s), None),
            NotEqual(n, s) => (Equal(n, s), None),
            LowerThan(n) => (GreaterThanOrEqual(n), None),
            LowerThanOrEqual(n) => (GreaterThan(n), None),
            Between(n, m) => (LowerThan(n), Some(GreaterThan(m))),
            GeoLowerThan(point, distance) => (GeoGreaterThan(point, distance), None),
            GeoGreaterThan(point, distance) => (GeoLowerThan(point, distance), None),
        }
    }
}

pub trait FilterParserError<'a>:
    nom::error::ParseError<&'a str> + ContextError<&'a str> + std::fmt::Debug
{
}

impl<'a> FilterParserError<'a> for VerboseError<&'a str> {}

pub struct ParseContext<'a> {
    pub fields_ids_map: &'a FieldsIdsMap,
    pub filterable_fields: &'a HashSet<String>,
}

impl<'a> ParseContext<'a> {
    /// and            = not (~ "AND" not)*
    fn parse_or<E>(&'a self, input: &'a str) -> IResult<&'a str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        let (input, lhs) = self.parse_and(input)?;
        let (input, ors) =
            many0(preceded(self.ws(tag("OR")), |c| Self::parse_and(self, c)))(input)?;

        let expr = ors
            .into_iter()
            .fold(lhs, |acc, branch| FilterCondition::Or(Box::new(acc), Box::new(branch)));
        Ok((input, expr))
    }

    fn parse_and<E>(&'a self, input: &'a str) -> IResult<&'a str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        let (input, lhs) = self.parse_not(input)?;
        let (input, ors) = many0(preceded(self.ws(tag("AND")), |c| self.parse_not(c)))(input)?;
        let expr = ors
            .into_iter()
            .fold(lhs, |acc, branch| FilterCondition::And(Box::new(acc), Box::new(branch)));
        Ok((input, expr))
    }

    /// not            = ("NOT" | "!") not | primary
    fn parse_not<E>(&'a self, input: &'a str) -> IResult<&'a str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        alt((
            map(preceded(alt((tag("!"), tag("NOT"))), |c| self.parse_not(c)), |e| e.negate()),
            |c| self.parse_primary(c),
        ))(input)
    }

    fn ws<F, O, E>(&'a self, inner: F) -> impl FnMut(&'a str) -> IResult<&'a str, O, E>
    where
        F: FnMut(&'a str) -> IResult<&'a str, O, E>,
        E: FilterParserError<'a>,
    {
        delimited(multispace0, inner, multispace0)
    }

    /// condition      = value ("==" | ">" ...) value
    fn parse_condition<E>(&'a self, input: &'a str) -> IResult<&'a str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        let operator = alt((tag("<="), tag(">="), tag("!="), tag("<"), tag(">"), tag("=")));
        let (input, (key, op, value)) =
            tuple((|c| self.parse_value(c), operator, |c| self.parse_value(c)))(input)?;

        let fid = self.parse_fid(input, key)?;
        let r: StdResult<f64, nom::Err<VerboseError<&str>>> = self.parse_numeric(value);
        match op {
            "=" => {
                let k =
                    FilterCondition::Operator(fid, Equal(r.ok(), value.to_string().to_lowercase()));
                Ok((input, k))
            }
            "!=" => {
                let k = FilterCondition::Operator(
                    fid,
                    NotEqual(r.ok(), value.to_string().to_lowercase()),
                );
                Ok((input, k))
            }
            ">" | "<" | "<=" | ">=" => {
                let numeric: f64 = self.parse_numeric(value)?;
                let k = match op {
                    ">" => FilterCondition::Operator(fid, GreaterThan(numeric)),
                    "<" => FilterCondition::Operator(fid, LowerThan(numeric)),
                    "<=" => FilterCondition::Operator(fid, LowerThanOrEqual(numeric)),
                    ">=" => FilterCondition::Operator(fid, GreaterThanOrEqual(numeric)),
                    _ => unreachable!(),
                };
                Ok((input, k))
            }
            _ => unreachable!(),
        }
    }

    fn parse_numeric<E, T>(&'a self, input: &'a str) -> StdResult<T, nom::Err<E>>
    where
        E: FilterParserError<'a>,
        T: std::str::FromStr,
    {
        match input.parse::<T>() {
            Ok(n) => Ok(n),
            Err(_) => match input.chars().nth(0) {
                Some(ch) => Err(nom::Err::Failure(E::from_char(input, ch))),
                None => Err(nom::Err::Failure(E::from_error_kind(input, ErrorKind::Eof))),
            },
        }
    }

    fn parse_fid<E>(&'a self, input: &'a str, key: &'a str) -> StdResult<FieldId, nom::Err<E>>
    where
        E: FilterParserError<'a>,
    {
        match self.fields_ids_map.id(key) {
            Some(fid) if self.filterable_fields.contains(key) => Ok(fid),
            _ => Err(nom::Err::Failure(E::add_context(
                input,
                "Attribute is not filterable",
                E::from_char(input, 'T'),
            ))),
        }
    }

    /// to             = value value TO value
    fn parse_to<E>(&'a self, input: &'a str) -> IResult<&'a str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        let (input, (key, from, _, to)) = tuple((
            self.ws(|c| self.parse_value(c)),
            self.ws(|c| self.parse_value(c)),
            tag("TO"),
            self.ws(|c| self.parse_value(c)),
        ))(input)?;

        let fid = self.parse_fid(input, key)?;
        let numeric_from: f64 = self.parse_numeric(from)?;
        let numeric_to: f64 = self.parse_numeric(to)?;
        let res = FilterCondition::Operator(fid, Between(numeric_from, numeric_to));

        Ok((input, res))
    }

    /// geoRadius      = WS* ~ "_geoRadius(float ~ "," ~ float ~ "," float)
    fn parse_geo_radius<E>(&'a self, input: &'a str) -> IResult<&'a str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        let err_msg_args_incomplete = "_geoRadius. The `_geoRadius` filter expect three arguments: `_geoRadius(latitude, longitude, radius)`";
        let err_msg_latitude_invalid =
            "_geoRadius. Latitude must be contained between -90 and 90 degrees.";

        let err_msg_longitude_invalid =
            "_geoRadius. Longitude must be contained between -180 and 180 degrees.";

        let parsed = preceded::<_, _, _, E, _, _>(
            // TODO: forbid spaces between _geoRadius and parenthesis
            self.ws(tag("_geoRadius")),
            delimited(
                char('('),
                separated_list1(tag(","), self.ws(|c| recognize_float(c))),
                char(')'),
            ),
        )(input);

        let (input, args): (&str, Vec<&str>) = match parsed {
            Ok(e) => e,
            Err(_e) => {
                return Err(nom::Err::Failure(E::add_context(
                    input,
                    err_msg_args_incomplete,
                    E::from_char(input, '('),
                )));
            }
        };

        if args.len() != 3 {
            let e = E::from_char(input, '(');
            return Err(nom::Err::Failure(E::add_context(input, err_msg_args_incomplete, e)));
        }
        let lat = self.parse_numeric(args[0])?;
        let lng = self.parse_numeric(args[1])?;
        let dis = self.parse_numeric(args[2])?;

        let fid = match self.fields_ids_map.id("_geo") {
            Some(fid) => fid,
            // TODO send an error
            None => return Ok((input, FilterCondition::Empty)),
        };

        if !(-90.0..=90.0).contains(&lat) {
            return Err(nom::Err::Failure(E::add_context(
                input,
                err_msg_latitude_invalid,
                E::from_char(input, '('),
            )));
        } else if !(-180.0..=180.0).contains(&lng) {
            return Err(nom::Err::Failure(E::add_context(
                input,
                err_msg_longitude_invalid,
                E::from_char(input, '('),
            )));
        }

        let res = FilterCondition::Operator(fid, GeoLowerThan([lat, lng], dis));
        Ok((input, res))
    }

    /// primary        = (WS* ~ "("  expression ")" ~ WS*) | condition | to | geoRadius
    fn parse_primary<E>(&'a self, input: &'a str) -> IResult<&str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        alt((
            delimited(self.ws(char('(')), |c| self.parse_expression(c), self.ws(char(')'))),
            |c| self.parse_condition(c),
            |c| self.parse_to(c),
            |c| self.parse_geo_radius(c),
        ))(input)
    }

    /// value          = WS* ~ ( word | singleQuoted | doubleQuoted) ~ WS*
    fn parse_value<E>(&'a self, input: &'a str) -> IResult<&'a str, &'a str, E>
    where
        E: FilterParserError<'a>,
    {
        // singleQuoted   = "'" .* all but quotes "'"
        let simple_quoted_key = |input| take_till(|c: char| c == '\'')(input);
        // doubleQuoted   = "\"" (word | spaces)* "\""
        let quoted_key = |input| take_till(|c: char| c == '"')(input);
        // word           = (alphanumeric | _ | - | .)+
        let word = |input| take_while1(Self::is_key_component)(input);

        alt((
            self.ws(delimited(char('\''), simple_quoted_key, char('\''))),
            self.ws(delimited(char('"'), quoted_key, char('"'))),
            self.ws(word),
        ))(input)
    }

    fn is_key_component(c: char) -> bool {
        c.is_alphanumeric() || ['_', '-', '.'].contains(&c)
    }

    /// expression     = or
    pub fn parse_expression<E>(&'a self, input: &'a str) -> IResult<&'a str, FilterCondition, E>
    where
        E: FilterParserError<'a>,
    {
        self.parse_or(input)
    }
}

#[cfg(test)]
mod tests {
    use big_s::S;
    use either::Either;
    use heed::EnvOpenOptions;
    use maplit::hashset;

    use super::*;
    use crate::update::Settings;
    use crate::Index;

    #[test]
    fn string() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut map = index.fields_ids_map(&wtxn).unwrap();
        map.insert("channel");
        map.insert("dog race");
        map.insert("subscribers");
        map.insert("_geo");
        index.put_fields_ids_map(&mut wtxn, &map).unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_filterable_fields(
            hashset! { S("channel"), S("dog race"), S("subscribers"), S("_geo") },
        );
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        use FilterCondition as Fc;
        let test_case = [
            // simple test
            (
                Fc::from_str(&rtxn, &index, "channel = Ponce"),
                Fc::Operator(0, Operator::Equal(None, S("ponce"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "subscribers = 12"),
                Fc::Operator(2, Operator::Equal(Some(12.), S("12"))),
            ),
            // test all the quotes and simple quotes
            (
                Fc::from_str(&rtxn, &index, "channel = 'Mister Mv'"),
                Fc::Operator(0, Operator::Equal(None, S("mister mv"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "channel = \"Mister Mv\""),
                Fc::Operator(0, Operator::Equal(None, S("mister mv"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "'dog race' = Borzoi"),
                Fc::Operator(1, Operator::Equal(None, S("borzoi"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "\"dog race\" = Chusky"),
                Fc::Operator(1, Operator::Equal(None, S("chusky"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "\"dog race\" = \"Bernese Mountain\""),
                Fc::Operator(1, Operator::Equal(None, S("bernese mountain"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "'dog race' = 'Bernese Mountain'"),
                Fc::Operator(1, Operator::Equal(None, S("bernese mountain"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "\"dog race\" = 'Bernese Mountain'"),
                Fc::Operator(1, Operator::Equal(None, S("bernese mountain"))),
            ),
            // test all the operators
            (
                Fc::from_str(&rtxn, &index, "channel != ponce"),
                Fc::Operator(0, Operator::NotEqual(None, S("ponce"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "NOT channel = ponce"),
                Fc::Operator(0, Operator::NotEqual(None, S("ponce"))),
            ),
            (
                Fc::from_str(&rtxn, &index, "subscribers < 1000"),
                Fc::Operator(2, Operator::LowerThan(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "subscribers > 1000"),
                Fc::Operator(2, Operator::GreaterThan(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "subscribers <= 1000"),
                Fc::Operator(2, Operator::LowerThanOrEqual(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "subscribers >= 1000"),
                Fc::Operator(2, Operator::GreaterThanOrEqual(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "NOT subscribers < 1000"),
                Fc::Operator(2, Operator::GreaterThanOrEqual(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "NOT subscribers > 1000"),
                Fc::Operator(2, Operator::LowerThanOrEqual(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "NOT subscribers <= 1000"),
                Fc::Operator(2, Operator::GreaterThan(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "NOT subscribers >= 1000"),
                Fc::Operator(2, Operator::LowerThan(1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "subscribers 100 TO 1000"),
                Fc::Operator(2, Operator::Between(100., 1000.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "NOT subscribers 100 TO 1000"),
                Fc::Or(
                    Box::new(Fc::Operator(2, Operator::LowerThan(100.))),
                    Box::new(Fc::Operator(2, Operator::GreaterThan(1000.))),
                ),
            ),
            (
                Fc::from_str(&rtxn, &index, "_geoRadius(12, 13, 14)"),
                Fc::Operator(3, Operator::GeoLowerThan([12., 13.], 14.)),
            ),
            (
                Fc::from_str(&rtxn, &index, "NOT _geoRadius(12, 13, 14)"),
                Fc::Operator(3, Operator::GeoGreaterThan([12., 13.], 14.)),
            ),
            // test simple `or` and `and`
            (
                Fc::from_str(&rtxn, &index, "channel = ponce AND 'dog race' != 'bernese mountain'"),
                Fc::And(
                    Box::new(Fc::Operator(0, Operator::Equal(None, S("ponce")))),
                    Box::new(Fc::Operator(1, Operator::NotEqual(None, S("bernese mountain")))),
                ),
            ),
            (
                Fc::from_str(&rtxn, &index, "channel = ponce OR 'dog race' != 'bernese mountain'"),
                Fc::Or(
                    Box::new(Fc::Operator(0, Operator::Equal(None, S("ponce")))),
                    Box::new(Fc::Operator(1, Operator::NotEqual(None, S("bernese mountain")))),
                ),
            ),
            (
                Fc::from_str(
                    &rtxn,
                    &index,
                    "channel = ponce AND 'dog race' != 'bernese mountain' OR subscribers > 1000",
                ),
                Fc::Or(
                    Box::new(Fc::And(
                        Box::new(Fc::Operator(0, Operator::Equal(None, S("ponce")))),
                        Box::new(Fc::Operator(1, Operator::NotEqual(None, S("bernese mountain")))),
                    )),
                    Box::new(Fc::Operator(2, Operator::GreaterThan(1000.))),
                ),
            ),
            // test parenthesis
            (
                Fc::from_str(
                    &rtxn,
                    &index,
                    "channel = ponce AND ( 'dog race' != 'bernese mountain' OR subscribers > 1000 )",
                ),
                    Fc::And(
                        Box::new(Fc::Operator(0, Operator::Equal(None, S("ponce")))),
                        Box::new(Fc::Or(
                            Box::new(Fc::Operator(1, Operator::NotEqual(None, S("bernese mountain")))),
                            Box::new(Fc::Operator(2, Operator::GreaterThan(1000.))),
                    ))),
            ),
            (
                Fc::from_str(
                    &rtxn,
                    &index,
                    "(channel = ponce AND 'dog race' != 'bernese mountain' OR subscribers > 1000) AND _geoRadius(12, 13, 14)",
                ),
                Fc::And(
                Box::new(Fc::Or(
                    Box::new(Fc::And(
                        Box::new(Fc::Operator(0, Operator::Equal(None, S("ponce")))),
                        Box::new(Fc::Operator(1, Operator::NotEqual(None, S("bernese mountain")))),
                    )),
                    Box::new(Fc::Operator(2, Operator::GreaterThan(1000.))),
                )),
                    Box::new(Fc::Operator(3, Operator::GeoLowerThan([12., 13.], 14.))))
            ),
        ];

        for (result, expected) in test_case {
            assert!(
                result.is_ok(),
                "Filter {:?} was supposed to be parsed but failed with the following error: `{}`",
                expected,
                result.unwrap_err()
            );
            let filter = result.unwrap();
            assert_eq!(filter, expected,);
        }
    }

    #[test]
    fn number() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut map = index.fields_ids_map(&wtxn).unwrap();
        map.insert("timestamp");
        index.put_fields_ids_map(&mut wtxn, &map).unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_filterable_fields(hashset! { "timestamp".into() });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        // Test that the facet condition is correctly generated.
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_str(&rtxn, &index, "timestamp 22 TO 44").unwrap();
        let expected = FilterCondition::Operator(0, Between(22.0, 44.0));
        assert_eq!(condition, expected);

        let condition = FilterCondition::from_str(&rtxn, &index, "NOT timestamp 22 TO 44").unwrap();
        let expected = FilterCondition::Or(
            Box::new(FilterCondition::Operator(0, LowerThan(22.0))),
            Box::new(FilterCondition::Operator(0, GreaterThan(44.0))),
        );
        assert_eq!(condition, expected);
    }

    #[test]
    fn compare() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("channel"), S("timestamp"), S("id")]); // to keep the fields order
        builder.set_filterable_fields(hashset! { S("channel"), S("timestamp") ,S("id")});
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_str(&rtxn, &index, "channel < 20").unwrap();
        let expected = FilterCondition::Operator(0, LowerThan(20.0));
        assert_eq!(condition, expected);

        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_str(&rtxn, &index, "id < 200").unwrap();
        let expected = FilterCondition::Operator(2, LowerThan(200.0));
        assert_eq!(condition, expected);
    }

    #[test]
    fn parentheses() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("channel"), S("timestamp")]); // to keep the fields order
        builder.set_filterable_fields(hashset! { S("channel"), S("timestamp") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        // Test that the facet condition is correctly generated.
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_str(
            &rtxn,
            &index,
            "channel = gotaga OR (timestamp 22 TO 44 AND channel != ponce)",
        )
        .unwrap();
        let expected = FilterCondition::Or(
            Box::new(FilterCondition::Operator(0, Operator::Equal(None, S("gotaga")))),
            Box::new(FilterCondition::And(
                Box::new(FilterCondition::Operator(1, Between(22.0, 44.0))),
                Box::new(FilterCondition::Operator(0, Operator::NotEqual(None, S("ponce")))),
            )),
        );
        assert_eq!(condition, expected);

        let condition = FilterCondition::from_str(
            &rtxn,
            &index,
            "channel = gotaga OR NOT (timestamp 22 TO 44 AND channel != ponce)",
        )
        .unwrap();
        let expected = FilterCondition::Or(
            Box::new(FilterCondition::Operator(0, Operator::Equal(None, S("gotaga")))),
            Box::new(FilterCondition::Or(
                Box::new(FilterCondition::Or(
                    Box::new(FilterCondition::Operator(1, LowerThan(22.0))),
                    Box::new(FilterCondition::Operator(1, GreaterThan(44.0))),
                )),
                Box::new(FilterCondition::Operator(0, Operator::Equal(None, S("ponce")))),
            )),
        );
        assert_eq!(condition, expected);
    }

    #[test]
    fn from_array() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("channel"), S("timestamp")]); // to keep the fields order
        builder.set_filterable_fields(hashset! { S("channel"), S("timestamp") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        // Simple array with Left
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array::<_, _, _, &str>(
            &rtxn,
            &index,
            vec![Either::Left(["channel = mv"])],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(&rtxn, &index, "channel = mv").unwrap();
        assert_eq!(condition, expected);

        // Simple array with Right
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array::<_, Option<&str>, _, _>(
            &rtxn,
            &index,
            vec![Either::Right("channel = mv")],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(&rtxn, &index, "channel = mv").unwrap();
        assert_eq!(condition, expected);

        // Array with Left and escaped quote
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array::<_, _, _, &str>(
            &rtxn,
            &index,
            vec![Either::Left(["channel = \"Mister Mv\""])],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(&rtxn, &index, "channel = \"Mister Mv\"").unwrap();
        assert_eq!(condition, expected);

        // Array with Right and escaped quote
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array::<_, Option<&str>, _, _>(
            &rtxn,
            &index,
            vec![Either::Right("channel = \"Mister Mv\"")],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(&rtxn, &index, "channel = \"Mister Mv\"").unwrap();
        assert_eq!(condition, expected);

        // Array with Left and escaped simple quote
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array::<_, _, _, &str>(
            &rtxn,
            &index,
            vec![Either::Left(["channel = 'Mister Mv'"])],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(&rtxn, &index, "channel = 'Mister Mv'").unwrap();
        assert_eq!(condition, expected);

        // Array with Right and escaped simple quote
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array::<_, Option<&str>, _, _>(
            &rtxn,
            &index,
            vec![Either::Right("channel = 'Mister Mv'")],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(&rtxn, &index, "channel = 'Mister Mv'").unwrap();
        assert_eq!(condition, expected);

        // Simple with parenthesis
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array::<_, _, _, &str>(
            &rtxn,
            &index,
            vec![Either::Left(["(channel = mv)"])],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(&rtxn, &index, "(channel = mv)").unwrap();
        assert_eq!(condition, expected);

        // Test that the facet condition is correctly generated.
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array(
            &rtxn,
            &index,
            vec![
                Either::Right("channel = gotaga"),
                Either::Left(vec!["timestamp = 44", "channel != ponce"]),
            ],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(
            &rtxn,
            &index,
            "channel = gotaga AND (timestamp = 44 OR channel != ponce)",
        )
        .unwrap();
        assert_eq!(condition, expected);
    }

    #[test]
    fn geo_radius() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("_geo"), S("price")]); // to keep the fields order
        builder.set_filterable_fields(hashset! { S("_geo"), S("price") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();
        // basic test
        let condition =
            FilterCondition::from_str(&rtxn, &index, "_geoRadius(12, 13.0005, 2000)").unwrap();
        let expected = FilterCondition::Operator(0, GeoLowerThan([12., 13.0005], 2000.));
        assert_eq!(condition, expected);

        // test the negation of the GeoLowerThan
        let condition =
            FilterCondition::from_str(&rtxn, &index, "NOT _geoRadius(50, 18, 2000.500)").unwrap();
        let expected = FilterCondition::Operator(0, GeoGreaterThan([50., 18.], 2000.500));
        assert_eq!(condition, expected);

        // composition of multiple operations
        let condition = FilterCondition::from_str(
            &rtxn,
            &index,
            "(NOT _geoRadius(1, 2, 300) AND _geoRadius(1.001, 2.002, 1000.300)) OR price <= 10",
        )
        .unwrap();
        let expected = FilterCondition::Or(
            Box::new(FilterCondition::And(
                Box::new(FilterCondition::Operator(0, GeoGreaterThan([1., 2.], 300.))),
                Box::new(FilterCondition::Operator(0, GeoLowerThan([1.001, 2.002], 1000.300))),
            )),
            Box::new(FilterCondition::Operator(1, LowerThanOrEqual(10.))),
        );
        assert_eq!(condition, expected);
    }

    #[test]
    fn geo_radius_error() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("_geo"), S("price")]); // to keep the fields order
        builder.set_filterable_fields(hashset! { S("_geo"), S("price") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();

        // georadius don't have any parameters
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains("The `_geoRadius` filter expect three arguments: `_geoRadius(latitude, longitude, radius)`"));

        // georadius don't have any parameters
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius()");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains("The `_geoRadius` filter expect three arguments: `_geoRadius(latitude, longitude, radius)`"));

        // georadius don't have enough parameters
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(1, 2)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains("The `_geoRadius` filter expect three arguments: `_geoRadius(latitude, longitude, radius)`"));

        // georadius have too many parameters
        let result =
            FilterCondition::from_str(&rtxn, &index, "_geoRadius(1, 2, 3, 4, 5, 6, 7, 8, 9, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains("The `_geoRadius` filter expect three arguments: `_geoRadius(latitude, longitude, radius)`"));

        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-100, 150, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("Latitude must be contained between -90 and 90 degrees."),
            "{}",
            error.to_string()
        );

        // georadius have a bad latitude
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-90.0000001, 150, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Latitude must be contained between -90 and 90 degrees."));

        // georadius have a bad longitude
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-10, 250, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Longitude must be contained between -180 and 180 degrees."));

        // georadius have a bad longitude
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-10, 180.000001, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Longitude must be contained between -180 and 180 degrees."));
    }
}
