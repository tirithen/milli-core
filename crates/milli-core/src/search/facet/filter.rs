use std::collections::BTreeSet;
use std::fmt::{Debug, Display};
use std::ops::Bound::{self, Excluded, Included, Unbounded};

pub use crate::filter_parser::{Condition, Error as FPError, FilterCondition, Token};
use either::Either;
use heed::types::LazyDecode;
use heed::BytesEncode;
use memchr::memmem::Finder;
use roaring::{MultiOps, RoaringBitmap};
use serde_json::Value;

use super::facet_range_search;
use crate::constants::RESERVED_GEO_FIELD_NAME;
use crate::error::{Error, UserError};
use crate::filterable_attributes_rules::{filtered_matching_patterns, matching_features};
use crate::heed_codec::facet::{
    FacetGroupKey, FacetGroupKeyCodec, FacetGroupValue, FacetGroupValueCodec,
};
use crate::index::db_name::FACET_ID_STRING_DOCIDS;
use crate::{
    distance_between_two_points, lat_lng_to_xyz, FieldId, FieldsIdsMap,
    FilterableAttributesFeatures, FilterableAttributesRule, Index, InternalError, Result,
    SerializationError,
};

/// The maximum number of filters the filter AST can process.
const MAX_FILTER_DEPTH: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter<'a> {
    condition: FilterCondition<'a>,
}

#[derive(Debug)]
pub enum BadGeoError {
    Lat(f64),
    Lng(f64),
    BoundingBoxTopIsBelowBottom(f64, f64),
}

impl std::error::Error for BadGeoError {}

impl Display for BadGeoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BoundingBoxTopIsBelowBottom(top, bottom) => {
                write!(f, "The top latitude `{top}` is below the bottom latitude `{bottom}`.")
            }
            Self::Lat(lat) => write!(
                f,
                "Bad latitude `{}`. Latitude must be contained between -90 and 90 degrees. ",
                lat
            ),
            Self::Lng(lng) => write!(
                f,
                "Bad longitude `{}`. Longitude must be contained between -180 and 180 degrees. ",
                lng
            ),
        }
    }
}

#[derive(Debug)]
enum FilterError<'a> {
    AttributeNotFilterable { attribute: &'a str, filterable_patterns: BTreeSet<&'a str> },
    ParseGeoError(BadGeoError),
    TooDeep,
}
impl std::error::Error for FilterError<'_> {}

impl From<BadGeoError> for FilterError<'_> {
    fn from(geo_error: BadGeoError) -> Self {
        FilterError::ParseGeoError(geo_error)
    }
}

impl Display for FilterError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AttributeNotFilterable { attribute, filterable_patterns } => {
                write!(f, "Attribute `{attribute}` is not filterable.")?;
                if filterable_patterns.is_empty() {
                    write!(f, " This index does not have configured filterable attributes.")
                } else {
                    write!(f, " Available filterable attribute patterns are: ")?;
                    let mut filterables_list =
                        filterable_patterns.iter().map(AsRef::as_ref).collect::<Vec<&str>>();
                    filterables_list.sort_unstable();
                    for (idx, filterable) in filterables_list.iter().enumerate() {
                        write!(f, "`{filterable}`")?;
                        if idx != filterables_list.len() - 1 {
                            write!(f, ", ")?;
                        }
                    }
                    write!(f, ".")
                }
            }
            Self::TooDeep => write!(
                f,
                "Too many filter conditions, can't process more than {} filters.",
                MAX_FILTER_DEPTH
            ),
            Self::ParseGeoError(error) => write!(f, "{}", error),
        }
    }
}

impl<'a> From<FPError<'a>> for Error {
    fn from(error: FPError<'a>) -> Self {
        Self::UserError(UserError::InvalidFilter(error.to_string()))
    }
}

impl<'a> From<Filter<'a>> for FilterCondition<'a> {
    fn from(f: Filter<'a>) -> Self {
        f.condition
    }
}

impl<'a> Filter<'a> {
    pub fn from_json(facets: &'a Value) -> Result<Option<Self>> {
        match facets {
            Value::String(expr) => {
                let condition = Filter::from_str(expr)?;
                Ok(condition)
            }
            Value::Array(arr) => Self::parse_filter_array(arr),
            v => Err(Error::UserError(UserError::InvalidFilterExpression(
                &["String", "Array"],
                v.clone(),
            ))),
        }
    }

    fn parse_filter_array(arr: &'a [Value]) -> Result<Option<Self>> {
        let mut ands = Vec::new();
        for value in arr {
            match value {
                Value::String(s) => ands.push(Either::Right(s.as_str())),
                Value::Array(arr) => {
                    let mut ors = Vec::new();
                    for value in arr {
                        match value {
                            Value::String(s) => ors.push(s.as_str()),
                            v => {
                                return Err(Error::UserError(UserError::InvalidFilterExpression(
                                    &["String"],
                                    v.clone(),
                                )))
                            }
                        }
                    }
                    ands.push(Either::Left(ors));
                }
                v => {
                    return Err(Error::UserError(UserError::InvalidFilterExpression(
                        &["String", "[String]"],
                        v.clone(),
                    )))
                }
            }
        }

        Filter::from_array(ands)
    }

    pub fn from_array<I, J>(array: I) -> Result<Option<Self>>
    where
        I: IntoIterator<Item = Either<J, &'a str>>,
        J: IntoIterator<Item = &'a str>,
    {
        let mut ands = vec![];

        for either in array {
            match either {
                Either::Left(array) => {
                    let mut ors = vec![];
                    for rule in array {
                        if let Some(filter) = Self::from_str(rule)? {
                            ors.push(filter.condition);
                        }
                    }

                    match ors.len() {
                        0 => (),
                        1 => ands.push(ors.pop().unwrap()),
                        _ => ands.push(FilterCondition::Or(ors)),
                    }
                }
                Either::Right(rule) => {
                    if let Some(filter) = Self::from_str(rule)? {
                        ands.push(filter.condition);
                    }
                }
            }
        }
        let and = if ands.is_empty() {
            return Ok(None);
        } else if ands.len() == 1 {
            ands.pop().unwrap()
        } else {
            FilterCondition::And(ands)
        };

        if let Some(token) = and.token_at_depth(MAX_FILTER_DEPTH) {
            return Err(token.as_external_error(FilterError::TooDeep).into());
        }

        Ok(Some(Self { condition: and }))
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(expression: &'a str) -> Result<Option<Self>> {
        let condition = match FilterCondition::parse(expression) {
            Ok(Some(fc)) => Ok(fc),
            Ok(None) => return Ok(None),
            Err(e) => Err(Error::UserError(UserError::InvalidFilter(e.to_string()))),
        }?;

        if let Some(token) = condition.token_at_depth(MAX_FILTER_DEPTH) {
            return Err(token.as_external_error(FilterError::TooDeep).into());
        }

        Ok(Some(Self { condition }))
    }

    pub fn use_contains_operator(&self) -> Option<&Token> {
        self.condition.use_contains_operator()
    }
}

impl<'a> Filter<'a> {
    pub fn evaluate(&self, rtxn: &heed::RoTxn<'_>, index: &Index) -> Result<RoaringBitmap> {
        // to avoid doing this for each recursive call we're going to do it ONCE ahead of time
        let fields_ids_map = index.fields_ids_map(rtxn)?;
        let filterable_attributes_rules = index.filterable_attributes_rules(rtxn)?;
        for fid in self.condition.fids(MAX_FILTER_DEPTH) {
            let attribute = fid.value();
            if matching_features(attribute, &filterable_attributes_rules)
                .is_some_and(|(_, features)| features.is_filterable())
            {
                continue;
            }

            // If the field is not filterable, return an error
            return Err(fid.as_external_error(FilterError::AttributeNotFilterable {
                attribute,
                filterable_patterns: filtered_matching_patterns(
                    &filterable_attributes_rules,
                    &|features| features.is_filterable(),
                ),
            }))?;
        }

        self.inner_evaluate(rtxn, index, &fields_ids_map, &filterable_attributes_rules, None)
    }

    fn evaluate_operator(
        rtxn: &heed::RoTxn<'_>,
        index: &Index,
        field_id: FieldId,
        universe: Option<&RoaringBitmap>,
        operator: &Condition<'a>,
        features: &FilterableAttributesFeatures,
        rule_index: usize,
    ) -> Result<RoaringBitmap> {
        let numbers_db = index.facet_id_f64_docids;
        let strings_db = index.facet_id_string_docids;

        // Make sure we always bound the ranges with the field id and the level,
        // as the facets values are all in the same database and prefixed by the
        // field id and the level.

        let (number_bounds, (left_str, right_str)) = match operator {
            // return an error if the filter is not allowed for this field
            Condition::GreaterThan(_)
            | Condition::GreaterThanOrEqual(_)
            | Condition::LowerThan(_)
            | Condition::LowerThanOrEqual(_)
            | Condition::Between { .. }
                if !features.is_filterable_comparison() =>
            {
                return Err(generate_filter_error(
                    rtxn, index, field_id, operator, features, rule_index,
                ));
            }
            Condition::Empty if !features.is_filterable_empty() => {
                return Err(generate_filter_error(
                    rtxn, index, field_id, operator, features, rule_index,
                ));
            }
            Condition::Null if !features.is_filterable_null() => {
                return Err(generate_filter_error(
                    rtxn, index, field_id, operator, features, rule_index,
                ));
            }
            Condition::Exists if !features.is_filterable_exists() => {
                return Err(generate_filter_error(
                    rtxn, index, field_id, operator, features, rule_index,
                ));
            }
            Condition::Equal(_) | Condition::NotEqual(_) if !features.is_filterable_equality() => {
                return Err(generate_filter_error(
                    rtxn, index, field_id, operator, features, rule_index,
                ));
            }
            Condition::GreaterThan(val) => {
                let number = val.parse_finite_float().ok();
                let number_bounds = number.map(|number| (Excluded(number), Included(f64::MAX)));
                let str_bounds = (Excluded(val.value()), Unbounded);
                (number_bounds, str_bounds)
            }
            Condition::GreaterThanOrEqual(val) => {
                let number = val.parse_finite_float().ok();
                let number_bounds = number.map(|number| (Included(number), Included(f64::MAX)));
                let str_bounds = (Included(val.value()), Unbounded);
                (number_bounds, str_bounds)
            }
            Condition::LowerThan(val) => {
                let number = val.parse_finite_float().ok();
                let number_bounds = number.map(|number| (Included(f64::MIN), Excluded(number)));
                let str_bounds = (Unbounded, Excluded(val.value()));
                (number_bounds, str_bounds)
            }
            Condition::LowerThanOrEqual(val) => {
                let number = val.parse_finite_float().ok();
                let number_bounds = number.map(|number| (Included(f64::MIN), Included(number)));
                let str_bounds = (Unbounded, Included(val.value()));
                (number_bounds, str_bounds)
            }
            Condition::Between { from, to } => {
                let from_number = from.parse_finite_float().ok();
                let to_number = to.parse_finite_float().ok();

                let number_bounds =
                    from_number.zip(to_number).map(|(from, to)| (Included(from), Included(to)));
                let str_bounds = (Included(from.value()), Included(to.value()));
                (number_bounds, str_bounds)
            }
            Condition::Null => {
                let is_null = index.null_faceted_documents_ids(rtxn, field_id)?;
                return Ok(is_null);
            }
            Condition::Empty => {
                let is_empty = index.empty_faceted_documents_ids(rtxn, field_id)?;
                return Ok(is_empty);
            }
            Condition::Exists => {
                let exist = index.exists_faceted_documents_ids(rtxn, field_id)?;
                return Ok(exist);
            }
            Condition::Equal(val) => {
                let string_docids = strings_db
                    .get(
                        rtxn,
                        &FacetGroupKey {
                            field_id,
                            level: 0,
                            left_bound: &crate::normalize_facet(val.value()),
                        },
                    )?
                    .map(|v| v.bitmap)
                    .unwrap_or_default();
                let number = val.parse_finite_float().ok();
                let number_docids = match number {
                    Some(n) => numbers_db
                        .get(rtxn, &FacetGroupKey { field_id, level: 0, left_bound: n })?
                        .map(|v| v.bitmap)
                        .unwrap_or_default(),
                    None => RoaringBitmap::new(),
                };
                return Ok(string_docids | number_docids);
            }
            Condition::NotEqual(val) => {
                let operator = Condition::Equal(val.clone());
                let docids = Self::evaluate_operator(
                    rtxn, index, field_id, None, &operator, features, rule_index,
                )?;
                let all_ids = index.documents_ids(rtxn)?;
                return Ok(all_ids - docids);
            }
            Condition::Contains { keyword: _, word } => {
                let value = crate::normalize_facet(word.value());
                let finder = Finder::new(&value);
                let base = FacetGroupKey { field_id, level: 0, left_bound: "" };
                let docids = strings_db
                    .prefix_iter(rtxn, &base)?
                    .remap_data_type::<LazyDecode<FacetGroupValueCodec>>()
                    .filter_map(|result| -> Option<Result<RoaringBitmap>> {
                        match result {
                            Ok((FacetGroupKey { left_bound, .. }, lazy_group_value)) => {
                                if finder.find(left_bound.as_bytes()).is_some() {
                                    Some(lazy_group_value.decode().map(|gv| gv.bitmap).map_err(
                                        |_| {
                                            InternalError::from(SerializationError::Decoding {
                                                db_name: Some(FACET_ID_STRING_DOCIDS),
                                            })
                                            .into()
                                        },
                                    ))
                                } else {
                                    None
                                }
                            }
                            Err(_e) => {
                                Some(Err(InternalError::from(SerializationError::Decoding {
                                    db_name: Some(FACET_ID_STRING_DOCIDS),
                                })
                                .into()))
                            }
                        }
                    })
                    .union()?;

                return Ok(docids);
            }
            Condition::StartsWith { keyword: _, word } => {
                let value = crate::normalize_facet(word.value());
                let base = FacetGroupKey { field_id, level: 0, left_bound: value.as_str() };
                let docids = strings_db
                    .prefix_iter(rtxn, &base)?
                    .map(|result| -> Result<RoaringBitmap> {
                        match result {
                            Ok((_facet_group_key, FacetGroupValue { bitmap, .. })) => Ok(bitmap),
                            Err(_e) => Err(InternalError::from(SerializationError::Decoding {
                                db_name: Some(FACET_ID_STRING_DOCIDS),
                            })
                            .into()),
                        }
                    })
                    .union()?;

                return Ok(docids);
            }
        };

        let mut output = RoaringBitmap::new();

        if let Some((left_number, right_number)) = number_bounds {
            Self::explore_facet_levels(
                rtxn,
                numbers_db,
                field_id,
                &left_number,
                &right_number,
                universe,
                &mut output,
            )?;
        }

        Self::explore_facet_levels(
            rtxn,
            strings_db,
            field_id,
            &left_str,
            &right_str,
            universe,
            &mut output,
        )?;

        Ok(output)
    }

    /// Aggregates the documents ids that are part of the specified range automatically
    /// going deeper through the levels.
    fn explore_facet_levels<'data, BoundCodec>(
        rtxn: &'data heed::RoTxn<'data>,
        db: heed::Database<FacetGroupKeyCodec<BoundCodec>, FacetGroupValueCodec>,
        field_id: FieldId,
        left: &'data Bound<<BoundCodec as heed::BytesEncode<'data>>::EItem>,
        right: &'data Bound<<BoundCodec as heed::BytesEncode<'data>>::EItem>,
        universe: Option<&RoaringBitmap>,
        output: &mut RoaringBitmap,
    ) -> Result<()>
    where
        BoundCodec: for<'b> BytesEncode<'b>,
        for<'b> <BoundCodec as BytesEncode<'b>>::EItem: Sized + PartialOrd,
    {
        match (left, right) {
            // lower TO upper when lower > upper must return no result
            (Included(l), Included(r)) if l > r => return Ok(()),
            (Included(l), Excluded(r)) if l >= r => return Ok(()),
            (Excluded(l), Excluded(r)) if l >= r => return Ok(()),
            (Excluded(l), Included(r)) if l >= r => return Ok(()),
            (_, _) => (),
        }
        facet_range_search::find_docids_of_facet_within_bounds::<BoundCodec>(
            rtxn, db, field_id, left, right, universe, output,
        )?;

        Ok(())
    }

    fn inner_evaluate(
        &self,
        rtxn: &heed::RoTxn<'_>,
        index: &Index,
        field_ids_map: &FieldsIdsMap,
        filterable_attribute_rules: &[FilterableAttributesRule],
        universe: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap> {
        if universe.is_some_and(|u| u.is_empty()) {
            return Ok(RoaringBitmap::new());
        }

        match &self.condition {
            FilterCondition::Not(f) => {
                let selected = Self::inner_evaluate(
                    &(f.as_ref().clone()).into(),
                    rtxn,
                    index,
                    field_ids_map,
                    filterable_attribute_rules,
                    universe,
                )?;
                match universe {
                    Some(universe) => Ok(universe - selected),
                    None => {
                        let all_ids = index.documents_ids(rtxn)?;
                        Ok(all_ids - selected)
                    }
                }
            }
            FilterCondition::In { fid, els } => {
                let Some(field_id) = field_ids_map.id(fid.value()) else {
                    return Ok(RoaringBitmap::new());
                };
                let Some((rule_index, features)) =
                    matching_features(fid.value(), filterable_attribute_rules)
                else {
                    return Ok(RoaringBitmap::new());
                };

                els.iter()
                    .map(|el| Condition::Equal(el.clone()))
                    .map(|op| {
                        Self::evaluate_operator(
                            rtxn, index, field_id, universe, &op, &features, rule_index,
                        )
                    })
                    .union()
            }
            FilterCondition::Condition { fid, op } => {
                let Some(field_id) = field_ids_map.id(fid.value()) else {
                    return Ok(RoaringBitmap::new());
                };
                let Some((rule_index, features)) =
                    matching_features(fid.value(), filterable_attribute_rules)
                else {
                    return Ok(RoaringBitmap::new());
                };

                Self::evaluate_operator(rtxn, index, field_id, universe, op, &features, rule_index)
            }
            FilterCondition::Or(subfilters) => subfilters
                .iter()
                .cloned()
                .map(|f| {
                    Self::inner_evaluate(
                        &f.into(),
                        rtxn,
                        index,
                        field_ids_map,
                        filterable_attribute_rules,
                        universe,
                    )
                })
                .union(),
            FilterCondition::And(subfilters) => {
                let mut subfilters_iter = subfilters.iter();
                if let Some(first_subfilter) = subfilters_iter.next() {
                    let mut bitmap = Self::inner_evaluate(
                        &(first_subfilter.clone()).into(),
                        rtxn,
                        index,
                        field_ids_map,
                        filterable_attribute_rules,
                        universe,
                    )?;
                    for f in subfilters_iter {
                        if bitmap.is_empty() {
                            return Ok(bitmap);
                        }
                        // TODO We are doing the intersections two times,
                        //      it could be more efficient
                        //      Can't I just replace this `&=` by an `=`?
                        bitmap &= Self::inner_evaluate(
                            &(f.clone()).into(),
                            rtxn,
                            index,
                            field_ids_map,
                            filterable_attribute_rules,
                            Some(&bitmap),
                        )?;
                    }
                    Ok(bitmap)
                } else {
                    Ok(RoaringBitmap::new())
                }
            }
            FilterCondition::GeoLowerThan { point, radius } => {
                if index.is_geo_filtering_enabled(rtxn)? {
                    let base_point: [f64; 2] =
                        [point[0].parse_finite_float()?, point[1].parse_finite_float()?];
                    if !(-90.0..=90.0).contains(&base_point[0]) {
                        return Err(point[0].as_external_error(BadGeoError::Lat(base_point[0])))?;
                    }
                    if !(-180.0..=180.0).contains(&base_point[1]) {
                        return Err(point[1].as_external_error(BadGeoError::Lng(base_point[1])))?;
                    }
                    let radius = radius.parse_finite_float()?;
                    let rtree = match index.geo_rtree(rtxn)? {
                        Some(rtree) => rtree,
                        None => return Ok(RoaringBitmap::new()),
                    };

                    let xyz_base_point = lat_lng_to_xyz(&base_point);

                    let result = rtree
                        .nearest_neighbor_iter(&xyz_base_point)
                        .take_while(|point| {
                            distance_between_two_points(&base_point, &point.data.1)
                                <= radius + f64::EPSILON
                        })
                        .map(|point| point.data.0)
                        .collect();

                    Ok(result)
                } else {
                    Err(point[0].as_external_error(FilterError::AttributeNotFilterable {
                        attribute: RESERVED_GEO_FIELD_NAME,
                        filterable_patterns: filtered_matching_patterns(
                            filterable_attribute_rules,
                            &|features| features.is_filterable(),
                        ),
                    }))?
                }
            }
            FilterCondition::GeoBoundingBox { top_right_point, bottom_left_point } => {
                if index.is_geo_filtering_enabled(rtxn)? {
                    let top_right: [f64; 2] = [
                        top_right_point[0].parse_finite_float()?,
                        top_right_point[1].parse_finite_float()?,
                    ];
                    let bottom_left: [f64; 2] = [
                        bottom_left_point[0].parse_finite_float()?,
                        bottom_left_point[1].parse_finite_float()?,
                    ];
                    if !(-90.0..=90.0).contains(&top_right[0]) {
                        return Err(
                            top_right_point[0].as_external_error(BadGeoError::Lat(top_right[0]))
                        )?;
                    }
                    if !(-180.0..=180.0).contains(&top_right[1]) {
                        return Err(
                            top_right_point[1].as_external_error(BadGeoError::Lng(top_right[1]))
                        )?;
                    }
                    if !(-90.0..=90.0).contains(&bottom_left[0]) {
                        return Err(bottom_left_point[0]
                            .as_external_error(BadGeoError::Lat(bottom_left[0])))?;
                    }
                    if !(-180.0..=180.0).contains(&bottom_left[1]) {
                        return Err(bottom_left_point[1]
                            .as_external_error(BadGeoError::Lng(bottom_left[1])))?;
                    }
                    if top_right[0] < bottom_left[0] {
                        return Err(bottom_left_point[1].as_external_error(
                            BadGeoError::BoundingBoxTopIsBelowBottom(top_right[0], bottom_left[0]),
                        ))?;
                    }

                    // Instead of writing a custom `GeoBoundingBox` filter we're simply going to re-use the range
                    // filter to create the following filter;
                    // `_geo.lat {top_right[0]} TO {bottom_left[0]} AND _geo.lng {top_right[1]} TO {bottom_left[1]}`
                    // As we can see, we need to use a bunch of tokens that don't exist in the original filter,
                    // thus we're going to create tokens that point to a random span but contain our text.

                    let geo_lat_token = Token::new(
                        top_right_point[0].original_span(),
                        Some("_geo.lat".to_string()),
                    );

                    let condition_lat = FilterCondition::Condition {
                        fid: geo_lat_token,
                        op: Condition::Between {
                            from: bottom_left_point[0].clone(),
                            to: top_right_point[0].clone(),
                        },
                    };

                    let selected_lat = Filter { condition: condition_lat }.inner_evaluate(
                        rtxn,
                        index,
                        field_ids_map,
                        filterable_attribute_rules,
                        universe,
                    )?;

                    let geo_lng_token = Token::new(
                        top_right_point[1].original_span(),
                        Some("_geo.lng".to_string()),
                    );
                    let selected_lng = if top_right[1] < bottom_left[1] {
                        // In this case the bounding box is wrapping around the earth (going from 180 to -180).
                        // We need to update the lng part of the filter from;
                        // `_geo.lng {top_right[1]} TO {bottom_left[1]}` to
                        // `_geo.lng {bottom_left[1]} TO 180 AND _geo.lng -180 TO {top_right[1]}`

                        let min_lng_token = Token::new(
                            top_right_point[1].original_span(),
                            Some("-180.0".to_string()),
                        );
                        let max_lng_token = Token::new(
                            top_right_point[1].original_span(),
                            Some("180.0".to_string()),
                        );

                        let condition_left = FilterCondition::Condition {
                            fid: geo_lng_token.clone(),
                            op: Condition::Between {
                                from: bottom_left_point[1].clone(),
                                to: max_lng_token,
                            },
                        };
                        let left = Filter { condition: condition_left }.inner_evaluate(
                            rtxn,
                            index,
                            field_ids_map,
                            filterable_attribute_rules,
                            universe,
                        )?;

                        let condition_right = FilterCondition::Condition {
                            fid: geo_lng_token,
                            op: Condition::Between {
                                from: min_lng_token,
                                to: top_right_point[1].clone(),
                            },
                        };
                        let right = Filter { condition: condition_right }.inner_evaluate(
                            rtxn,
                            index,
                            field_ids_map,
                            filterable_attribute_rules,
                            universe,
                        )?;

                        left | right
                    } else {
                        let condition_lng = FilterCondition::Condition {
                            fid: geo_lng_token,
                            op: Condition::Between {
                                from: bottom_left_point[1].clone(),
                                to: top_right_point[1].clone(),
                            },
                        };
                        Filter { condition: condition_lng }.inner_evaluate(
                            rtxn,
                            index,
                            field_ids_map,
                            filterable_attribute_rules,
                            universe,
                        )?
                    };

                    Ok(selected_lat & selected_lng)
                } else {
                    Err(top_right_point[0].as_external_error(
                        FilterError::AttributeNotFilterable {
                            attribute: RESERVED_GEO_FIELD_NAME,
                            filterable_patterns: filtered_matching_patterns(
                                filterable_attribute_rules,
                                &|features| features.is_filterable(),
                            ),
                        },
                    ))?
                }
            }
        }
    }
}

fn generate_filter_error(
    rtxn: &heed::RoTxn<'_>,
    index: &Index,
    field_id: FieldId,
    operator: &Condition<'_>,
    features: &FilterableAttributesFeatures,
    rule_index: usize,
) -> Error {
    match index.fields_ids_map(rtxn) {
        Ok(fields_ids_map) => {
            let field = fields_ids_map.name(field_id).unwrap_or_default();
            Error::UserError(UserError::FilterOperatorNotAllowed {
                field: field.to_string(),
                allowed_operators: features.allowed_filter_operators(),
                operator: operator.operator().to_string(),
                rule_index,
            })
        }
        Err(e) => e.into(),
    }
}

impl<'a> From<FilterCondition<'a>> for Filter<'a> {
    fn from(fc: FilterCondition<'a>) -> Self {
        Self { condition: fc }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;
    use std::iter::FromIterator;

    use big_s::S;
    use either::Either;
    use meili_snap::snapshot;
    use roaring::RoaringBitmap;

    use crate::constants::RESERVED_GEO_FIELD_NAME;
    use crate::index::tests::TempIndex;
    use crate::{Filter, FilterableAttributesRule};

    #[test]
    fn empty_db() {
        let index = TempIndex::new();
        //Set the filterable fields to be the channel.
        index
            .update_settings(|settings| {
                settings.set_filterable_fields(vec![FilterableAttributesRule::Field(
                    "PrIcE".to_string(),
                )]);
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();

        let filter = Filter::from_str("PrIcE < 1000").unwrap().unwrap();
        let bitmap = filter.evaluate(&rtxn, &index).unwrap();
        assert!(bitmap.is_empty());

        let filter = Filter::from_str("NOT PrIcE >= 1000").unwrap().unwrap();
        let bitmap = filter.evaluate(&rtxn, &index).unwrap();
        assert!(bitmap.is_empty());
    }

    #[test]
    fn from_array() {
        // Simple array with Left
        let condition = Filter::from_array(vec![Either::Left(["channel = mv"])]).unwrap().unwrap();
        let expected = Filter::from_str("channel = mv").unwrap().unwrap();
        assert_eq!(condition, expected);

        // Simple array with Right
        let condition = Filter::from_array::<_, Option<&str>>(vec![Either::Right("channel = mv")])
            .unwrap()
            .unwrap();
        let expected = Filter::from_str("channel = mv").unwrap().unwrap();
        assert_eq!(condition, expected);

        // Array with Left and escaped quote
        let condition =
            Filter::from_array(vec![Either::Left(["channel = \"Mister Mv\""])]).unwrap().unwrap();
        let expected = Filter::from_str("channel = \"Mister Mv\"").unwrap().unwrap();
        assert_eq!(condition, expected);

        // Array with Right and escaped quote
        let condition =
            Filter::from_array::<_, Option<&str>>(vec![Either::Right("channel = \"Mister Mv\"")])
                .unwrap()
                .unwrap();
        let expected = Filter::from_str("channel = \"Mister Mv\"").unwrap().unwrap();
        assert_eq!(condition, expected);

        // Array with Left and escaped simple quote
        let condition =
            Filter::from_array(vec![Either::Left(["channel = 'Mister Mv'"])]).unwrap().unwrap();
        let expected = Filter::from_str("channel = 'Mister Mv'").unwrap().unwrap();
        assert_eq!(condition, expected);

        // Array with Right and escaped simple quote
        let condition =
            Filter::from_array::<_, Option<&str>>(vec![Either::Right("channel = 'Mister Mv'")])
                .unwrap()
                .unwrap();
        let expected = Filter::from_str("channel = 'Mister Mv'").unwrap().unwrap();
        assert_eq!(condition, expected);

        // Simple with parenthesis
        let condition =
            Filter::from_array(vec![Either::Left(["(channel = mv)"])]).unwrap().unwrap();
        let expected = Filter::from_str("(channel = mv)").unwrap().unwrap();
        assert_eq!(condition, expected);

        // Test that the facet condition is correctly generated.
        let condition = Filter::from_array(vec![
            Either::Right("channel = gotaga"),
            Either::Left(vec!["timestamp = 44", "channel != ponce"]),
        ])
        .unwrap()
        .unwrap();
        let expected =
            Filter::from_str("channel = gotaga AND (timestamp = 44 OR channel != ponce)")
                .unwrap()
                .unwrap();
        assert_eq!(condition, expected);
    }

    #[test]
    fn not_filterable() {
        let index = TempIndex::new();

        let rtxn = index.read_txn().unwrap();
        let filter = Filter::from_str("_geoRadius(42, 150, 10)").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `_geo` is not filterable. This index does not have configured filterable attributes.
        12:14 _geoRadius(42, 150, 10)
        "###);

        let filter = Filter::from_str("_geoBoundingBox([42, 150], [30, 10])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `_geo` is not filterable. This index does not have configured filterable attributes.
        18:20 _geoBoundingBox([42, 150], [30, 10])
        "###);

        let filter = Filter::from_str("dog = \"bernese mountain\"").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `dog` is not filterable. This index does not have configured filterable attributes.
        1:4 dog = "bernese mountain"
        "###);
        drop(rtxn);

        index
            .update_settings(|settings| {
                settings.set_searchable_fields(vec![S("title")]);
                settings.set_filterable_fields(vec![FilterableAttributesRule::Field(
                    "title".to_string(),
                )]);
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();

        let filter = Filter::from_str("_geoRadius(-100, 150, 10)").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `_geo` is not filterable. Available filterable attribute patterns are: `title`.
        12:16 _geoRadius(-100, 150, 10)
        "###);

        let filter = Filter::from_str("_geoBoundingBox([42, 150], [30, 10])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `_geo` is not filterable. Available filterable attribute patterns are: `title`.
        18:20 _geoBoundingBox([42, 150], [30, 10])
        "###);

        let filter = Filter::from_str("name = 12").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `name` is not filterable. Available filterable attribute patterns are: `title`.
        1:5 name = 12
        "###);

        let filter = Filter::from_str("title = \"test\" AND name = 12").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `name` is not filterable. Available filterable attribute patterns are: `title`.
        20:24 title = "test" AND name = 12
        "###);

        let filter = Filter::from_str("title = \"test\" AND name IN [12]").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `name` is not filterable. Available filterable attribute patterns are: `title`.
        20:24 title = "test" AND name IN [12]
        "###);

        let filter = Filter::from_str("title = \"test\" AND name != 12").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        snapshot!(error.to_string(), @r###"
        Attribute `name` is not filterable. Available filterable attribute patterns are: `title`.
        20:24 title = "test" AND name != 12
        "###);
    }

    #[test]
    fn escaped_quote_in_filter_value_2380() {
        let index = TempIndex::new();

        index
            .add_documents(documents!([
                {
                    "id": "test_1",
                    "monitor_diagonal": "27' to 30'"
                },
                {
                    "id": "test_2",
                    "monitor_diagonal": "27\" to 30\""
                },
                {
                    "id": "test_3",
                    "monitor_diagonal": "27\" to 30'"
                },
            ]))
            .unwrap();

        index
            .update_settings(|settings| {
                settings.set_filterable_fields(vec![FilterableAttributesRule::Field(
                    "monitor_diagonal".to_string(),
                )]);
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();

        let mut search = crate::Search::new(&rtxn, &index);
        // this filter is copy pasted from #2380 with the exact same espace sequence
        search.filter(Filter::from_str("monitor_diagonal = '27\" to 30\\''").unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![2]);

        search.filter(Filter::from_str(r#"monitor_diagonal = "27' to 30'" "#).unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![0]);

        search.filter(Filter::from_str(r#"monitor_diagonal = "27\" to 30\"" "#).unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![1]);

        search.filter(Filter::from_str(r#"monitor_diagonal = "27\" to 30'" "#).unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![2]);
    }

    #[test]
    fn zero_radius() {
        let index = TempIndex::new();

        index
            .update_settings(|settings| {
                settings.set_filterable_fields(vec![FilterableAttributesRule::Field(S(
                    RESERVED_GEO_FIELD_NAME,
                ))]);
            })
            .unwrap();

        index
            .add_documents(documents!([
              {
                "id": 1,
                "name": "Nàpiz' Milano",
                "address": "Viale Vittorio Veneto, 30, 20124, Milan, Italy",
                "type": "pizza",
                "rating": 9,
                RESERVED_GEO_FIELD_NAME: {
                  "lat": 45.4777599,
                  "lng": 9.1967508
                }
              },
              {
                "id": 2,
                "name": "Artico Gelateria Tradizionale",
                "address": "Via Dogana, 1, 20123 Milan, Italy",
                "type": "ice cream",
                "rating": 10,
                RESERVED_GEO_FIELD_NAME: {
                  "lat": 45.4632046,
                  "lng": 9.1719421
                }
              },
            ]))
            .unwrap();

        let rtxn = index.read_txn().unwrap();

        let mut search = crate::Search::new(&rtxn, &index);

        search.filter(Filter::from_str("_geoRadius(45.4777599, 9.1967508, 0)").unwrap().unwrap());
        let crate::SearchResult { documents_ids, .. } = search.execute().unwrap();
        assert_eq!(documents_ids, vec![0]);
    }

    #[test]
    fn geo_radius_error() {
        let index = TempIndex::new();

        index
            .update_settings(|settings| {
                settings.set_searchable_fields(vec![S(RESERVED_GEO_FIELD_NAME), S("price")]); // to keep the fields order
                settings.set_filterable_fields(vec![
                    FilterableAttributesRule::Field(S(RESERVED_GEO_FIELD_NAME)),
                    FilterableAttributesRule::Field("price".to_string()),
                ]);
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();

        // georadius have a bad latitude
        let filter = Filter::from_str("_geoRadius(-100, 150, 10)").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(
            error.to_string().starts_with(
                "Bad latitude `-100`. Latitude must be contained between -90 and 90 degrees."
            ),
            "{}",
            error.to_string()
        );

        // georadius have a bad latitude
        let filter = Filter::from_str("_geoRadius(-90.0000001, 150, 10)").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad latitude `-90.0000001`. Latitude must be contained between -90 and 90 degrees."
        ));

        // georadius have a bad longitude
        let filter = Filter::from_str("_geoRadius(-10, 250, 10)").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(
            error.to_string().contains(
                "Bad longitude `250`. Longitude must be contained between -180 and 180 degrees."
            ),
            "{}",
            error.to_string(),
        );

        // georadius have a bad longitude
        let filter = Filter::from_str("_geoRadius(-10, 180.000001, 10)").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad longitude `180.000001`. Longitude must be contained between -180 and 180 degrees."
        ));
    }

    #[test]
    fn geo_bounding_box_error() {
        let index = TempIndex::new();

        index
            .update_settings(|settings| {
                settings.set_searchable_fields(vec![S(RESERVED_GEO_FIELD_NAME), S("price")]); // to keep the fields order
                settings.set_filterable_fields(vec![
                    FilterableAttributesRule::Field(S(RESERVED_GEO_FIELD_NAME)),
                    FilterableAttributesRule::Field("price".to_string()),
                ]);
            })
            .unwrap();

        let rtxn = index.read_txn().unwrap();

        // geoboundingbox top left coord have a bad latitude
        let filter =
            Filter::from_str("_geoBoundingBox([-90.0000001, 150], [30, 10])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(
            error.to_string().starts_with(
                "Bad latitude `-90.0000001`. Latitude must be contained between -90 and 90 degrees."
            ),
            "{}",
            error.to_string()
        );

        // geoboundingbox top left coord have a bad latitude
        let filter =
            Filter::from_str("_geoBoundingBox([90.0000001, 150], [30, 10])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(
            error.to_string().starts_with(
                "Bad latitude `90.0000001`. Latitude must be contained between -90 and 90 degrees."
            ),
            "{}",
            error.to_string()
        );

        // geoboundingbox bottom right coord have a bad latitude
        let filter =
            Filter::from_str("_geoBoundingBox([30, 10], [-90.0000001, 150])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad latitude `-90.0000001`. Latitude must be contained between -90 and 90 degrees."
        ));

        // geoboundingbox bottom right coord have a bad latitude
        let filter =
            Filter::from_str("_geoBoundingBox([30, 10], [90.0000001, 150])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad latitude `90.0000001`. Latitude must be contained between -90 and 90 degrees."
        ));

        // geoboundingbox top left coord have a bad longitude
        let filter =
            Filter::from_str("_geoBoundingBox([-10, 180.000001], [30, 10])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad longitude `180.000001`. Longitude must be contained between -180 and 180 degrees."
        ));

        // geoboundingbox top left coord have a bad longitude
        let filter =
            Filter::from_str("_geoBoundingBox([-10, -180.000001], [30, 10])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad longitude `-180.000001`. Longitude must be contained between -180 and 180 degrees."
        ));

        // geoboundingbox bottom right coord have a bad longitude
        let filter =
            Filter::from_str("_geoBoundingBox([30, 10], [-10, -180.000001])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad longitude `-180.000001`. Longitude must be contained between -180 and 180 degrees."
        ));

        // geoboundingbox bottom right coord have a bad longitude
        let filter =
            Filter::from_str("_geoBoundingBox([30, 10], [-10, 180.000001])").unwrap().unwrap();
        let error = filter.evaluate(&rtxn, &index).unwrap_err();
        assert!(error.to_string().contains(
            "Bad longitude `180.000001`. Longitude must be contained between -180 and 180 degrees."
        ));
    }

    #[test]
    fn filter_depth() {
        // generates a big (2 MiB) filter with too much of ORs.
        let tipic_filter = "account_ids=14361 OR ";
        let mut filter_string = String::with_capacity(tipic_filter.len() * 14360);
        for i in 1..=14361 {
            let _ = write!(&mut filter_string, "account_ids={}", i);
            if i != 14361 {
                let _ = write!(&mut filter_string, " OR ");
            }
        }

        // Note: the filter used to be rejected for being too deep, but that is
        // no longer the case
        let filter = Filter::from_str(&filter_string).unwrap();
        assert!(filter.is_some());
    }

    #[test]
    fn empty_filter() {
        let option = Filter::from_str("     ").unwrap();
        assert_eq!(option, None);
    }

    #[test]
    fn non_finite_float() {
        let index = TempIndex::new();

        index
            .update_settings(|settings| {
                settings.set_searchable_fields(vec![S("price")]); // to keep the fields order
                settings.set_filterable_fields(vec![FilterableAttributesRule::Field(
                    "price".to_string(),
                )]);
            })
            .unwrap();
        index
            .add_documents(documents!([
                {
                    "id": "test_1",
                    "price": "inf"
                },
                {
                    "id": "test_2",
                    "price": "2000"
                },
                {
                    "id": "test_3",
                    "price": "infinity"
                },
            ]))
            .unwrap();

        let rtxn = index.read_txn().unwrap();
        let filter = Filter::from_str("price = inf").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert!(result.contains(0));
        let filter = Filter::from_str("price < inf").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        // this is allowed due to filters with strings
        assert!(result.contains(1));

        let filter = Filter::from_str("price = NaN").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert!(result.is_empty());
        let filter = Filter::from_str("price < NaN").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert!(result.contains(1));

        let filter = Filter::from_str("price = infinity").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert!(result.contains(2));
        let filter = Filter::from_str("price < infinity").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert!(result.contains(0));
        assert!(result.contains(1));
    }

    #[test]
    fn filter_number() {
        let index = TempIndex::new();

        index
            .update_settings(|settings| {
                settings.set_primary_key("id".to_owned());
                settings.set_filterable_fields(vec![
                    FilterableAttributesRule::Field("id".to_string()),
                    FilterableAttributesRule::Field("one".to_string()),
                    FilterableAttributesRule::Field("two".to_string()),
                ]);
            })
            .unwrap();

        let mut docs = vec![];
        for i in 0..100 {
            docs.push(serde_json::json!({ "id": i, "two": i % 10 }));
        }

        index.add_documents(documents!(docs)).unwrap();

        let rtxn = index.read_txn().unwrap();
        for i in 0..100 {
            let filter_str = format!("id = {i}");
            let filter = Filter::from_str(&filter_str).unwrap().unwrap();
            let result = filter.evaluate(&rtxn, &index).unwrap();
            assert_eq!(result, RoaringBitmap::from_iter([i]));
        }
        for i in 0..100 {
            let filter_str = format!("id > {i}");
            let filter = Filter::from_str(&filter_str).unwrap().unwrap();
            let result = filter.evaluate(&rtxn, &index).unwrap();
            assert_eq!(result, RoaringBitmap::from_iter((i + 1)..100));
        }
        for i in 0..100 {
            let filter_str = format!("id < {i}");
            let filter = Filter::from_str(&filter_str).unwrap().unwrap();
            let result = filter.evaluate(&rtxn, &index).unwrap();
            assert_eq!(result, RoaringBitmap::from_iter(0..i));
        }
        for i in 0..100 {
            let filter_str = format!("id <= {i}");
            let filter = Filter::from_str(&filter_str).unwrap().unwrap();
            let result = filter.evaluate(&rtxn, &index).unwrap();
            assert_eq!(result, RoaringBitmap::from_iter(0..=i));
        }
        for i in 0..100 {
            let filter_str = format!("id >= {i}");
            let filter = Filter::from_str(&filter_str).unwrap().unwrap();
            let result = filter.evaluate(&rtxn, &index).unwrap();
            assert_eq!(result, RoaringBitmap::from_iter(i..100));
        }
        for i in 0..100 {
            for j in i..100 {
                let filter_str = format!("id {i} TO {j}");
                let filter = Filter::from_str(&filter_str).unwrap().unwrap();
                let result = filter.evaluate(&rtxn, &index).unwrap();
                assert_eq!(result, RoaringBitmap::from_iter(i..=j));
            }
        }
        let filter = Filter::from_str("one >= 0 OR one <= 0").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert_eq!(result, RoaringBitmap::default());

        let filter = Filter::from_str("one = 0").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert_eq!(result, RoaringBitmap::default());

        for i in 0..10 {
            for j in i..10 {
                let filter_str = format!("two {i} TO {j}");
                let filter = Filter::from_str(&filter_str).unwrap().unwrap();
                let result = filter.evaluate(&rtxn, &index).unwrap();
                assert_eq!(
                    result,
                    RoaringBitmap::from_iter((0..100).filter(|x| (i..=j).contains(&(x % 10))))
                );
            }
        }
        let filter = Filter::from_str("two != 0").unwrap().unwrap();
        let result = filter.evaluate(&rtxn, &index).unwrap();
        assert_eq!(result, RoaringBitmap::from_iter((0..100).filter(|x| x % 10 != 0)));
    }
}
