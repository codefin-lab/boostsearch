//! Where a point is, and whether it is inside a shape.

use super::*;

/// The geo clause of a query: the field it reads and the shape it asks about.
pub(crate) fn find_geo_clause(node: &Value) -> Option<(String, Value)> {
    match node {
        Value::Object(o) => {
            for kind in ["geo_shape", "geo_bounding_box", "geo_distance", "geo_polygon"] {
                if let Some(spec) = o.get(kind).and_then(|v| v.as_object()) {
                    let field = spec.keys().map(|k| k.to_string()).find(|k| {
                        !matches!(
                            k.as_str(),
                            "boost"
                                | "_name"
                                | "ignore_unmapped"
                                | "validation_method"
                                | "type"
                                | "distance"
                                | "distance_type"
                                | "relation"
                        )
                    })?;
                    let mut shape = json!({"__kind": kind, "__spec": spec.get(&field)});
                    // a distance query keeps the radius beside the field
                    if let Some(d) = spec.get("distance") {
                        shape["__distance"] = d.clone();
                    }
                    return Some((field, shape));
                }
            }
            o.values().find_map(find_geo_clause)
        }
        Value::Array(a) => a.iter().find_map(find_geo_clause),
        _ => None,
    }
}

/// Is this point inside the shape the query named?
pub(crate) fn point_within(shape: &Value, point: &Value) -> bool {
    let Some((lat, lon)) = read_point(point) else { return false };
    let kind = shape.get("__kind").and_then(|k| k.as_str()).unwrap_or("");
    let spec = shape.get("__spec").cloned().unwrap_or(Value::Null);
    match kind {
        "geo_bounding_box" => {
            let corner = |name: &str| spec.get(name).and_then(read_point);
            match (corner("top_left"), corner("bottom_right")) {
                (Some((t, l)), Some((b, r))) => lat <= t && lat >= b && lon >= l && lon <= r,
                _ => false,
            }
        }
        "geo_distance" => {
            let radius = shape
                .get("__distance")
                .and_then(|d| d.as_str())
                .and_then(parse_distance)
                .unwrap_or(0.0);
            geo_distance_metres(&spec, point).map(|d| d <= radius).unwrap_or(false)
        }
        "geo_polygon" => {
            let points: Vec<(f64, f64)> = spec
                .get("points")
                .and_then(|p| p.as_array())
                .map(|a| a.iter().filter_map(read_point).collect())
                .unwrap_or_default();
            inside_polygon(&points, lat, lon)
        }
        _ => {
            // a shape: an envelope is two corners, a polygon a ring of points
            let shape = spec.get("shape").or_else(|| spec.get("indexed_shape")).unwrap_or(&spec);
            let coords = shape.get("coordinates");
            match shape.get("type").and_then(|t| t.as_str()).map(|t| t.to_lowercase()) {
                Some(ref t) if t == "envelope" => {
                    let Some(a) = coords.and_then(|c| c.as_array()) else { return false };
                    let corner = |i: usize| -> Option<(f64, f64)> {
                        let p = a.get(i)?.as_array()?;
                        Some((p.get(1)?.as_f64()?, p.first()?.as_f64()?))
                    };
                    match (corner(0), corner(1)) {
                        (Some((t, l)), Some((b, r))) => {
                            lat <= t && lat >= b && lon >= l && lon <= r
                        }
                        _ => false,
                    }
                }
                Some(ref t) if t == "polygon" => {
                    let ring: Vec<(f64, f64)> = coords
                        .and_then(|c| c.as_array())
                        .and_then(|a| a.first())
                        .and_then(|r| r.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|p| {
                                    let p = p.as_array()?;
                                    Some((p.get(1)?.as_f64()?, p.first()?.as_f64()?))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    inside_polygon(&ring, lat, lon)
                }
                Some(ref t) if t == "point" => {
                    let p = coords.and_then(|c| c.as_array()).map(|a| {
                        (
                            a.get(1).and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
                            a.first().and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
                        )
                    });
                    p.map(|(la, lo)| (la - lat).abs() < 1e-9 && (lo - lon).abs() < 1e-9)
                        .unwrap_or(false)
                }
                _ => false,
            }
        }
    }
}

/// A point, however it was written: a pair, an object, or text.
pub(crate) fn read_point(v: &Value) -> Option<(f64, f64)> {
    match v {
        // a pair is longitude first
        Value::Array(a) if a.len() == 2 => Some((a[1].as_f64()?, a[0].as_f64()?)),
        Value::Object(o) => {
            if let (Some(lat), Some(lon)) =
                (o.get("lat").and_then(|x| x.as_f64()), o.get("lon").and_then(|x| x.as_f64()))
            {
                return Some((lat, lon));
            }
            // written the way GeoJSON writes it: longitude first
            let c = o.get("coordinates")?.as_array()?;
            Some((c.get(1)?.as_f64()?, c.first()?.as_f64()?))
        }
        Value::String(s) => {
            let s = s.trim();
            if let Some(rest) = s.strip_prefix("POINT") {
                let inner = rest.trim().trim_start_matches('(').trim_end_matches(')');
                let mut parts = inner.split_whitespace();
                let lon: f64 = parts.next()?.parse().ok()?;
                let lat: f64 = parts.next()?.parse().ok()?;
                return Some((lat, lon));
            }
            if let Some((a, b)) = s.split_once(',') {
                return Some((a.trim().parse().ok()?, b.trim().parse().ok()?));
            }
            decode_geohash(s)
        }
        _ => None,
    }
}

/// A geohash is a box, narrowed a bit by each character; the point it stands
/// for is the middle of the box it ends at.
pub(crate) fn decode_geohash(hash: &str) -> Option<(f64, f64)> {
    const DIGITS: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let (mut lat_lo, mut lat_hi) = (-90.0f64, 90.0f64);
    let (mut lon_lo, mut lon_hi) = (-180.0f64, 180.0f64);
    let mut even = true;
    for c in hash.bytes() {
        let idx = DIGITS.iter().position(|d| *d == c.to_ascii_lowercase())? as u8;
        for bit in (0..5).rev() {
            let on = idx & (1 << bit) != 0;
            if even {
                let mid = (lon_lo + lon_hi) / 2.0;
                if on {
                    lon_lo = mid;
                } else {
                    lon_hi = mid;
                }
            } else {
                let mid = (lat_lo + lat_hi) / 2.0;
                if on {
                    lat_lo = mid;
                } else {
                    lat_hi = mid;
                }
            }
            even = !even;
        }
    }
    Some(((lat_lo + lat_hi) / 2.0, (lon_lo + lon_hi) / 2.0))
}

/// The even-odd rule: a point is inside a ring when a ray from it crosses the
/// ring an odd number of times.
pub(crate) fn inside_polygon(ring: &[(f64, f64)], lat: f64, lon: f64) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (yi, xi) = ring[i];
        let (yj, xj) = ring[j];
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi + f64::EPSILON) + xi
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// A distance, written the way a pivot is.
pub(crate) fn parse_distance(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    Some(
        n * match unit.trim() {
            "m" => 1.0,
            "km" => 1000.0,
            "cm" => 0.01,
            "mm" => 0.001,
            "mi" => 1609.344,
            "ft" => 0.3048,
            _ => return None,
        },
    )
}

/// Metres between two points on the earth, by the haversine formula.
pub(crate) fn geo_distance_metres(origin: &Value, value: &Value) -> Option<f64> {
    let point = |v: &Value| -> Option<(f64, f64)> {
        match v {
            // a point written as a pair is longitude first
            Value::Array(a) if a.len() == 2 => Some((a[1].as_f64()?, a[0].as_f64()?)),
            Value::Object(o) => Some((
                o.get("lat").and_then(|x| x.as_f64())?,
                o.get("lon").and_then(|x| x.as_f64())?,
            )),
            Value::String(s) => {
                let (a, b) = s.split_once(',')?;
                Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
            }
            _ => None,
        }
    };
    let (lat1, lon1) = point(origin)?;
    let (lat2, lon2) = point(value)?;
    let r = 6_371_008.8f64;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    Some(2.0 * r * a.sqrt().asin())
}

/// `geo_distance`: buckets of how far each document is from a point.
///
/// The distance is not a column, so it is worked out from each document's own
/// position, and the documents in a bucket are then named to whatever
/// aggregations sit under it.
pub(crate) fn run_geo_distance_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    weighted: bool,
) -> std::result::Result<Value, Response> {
    let spec = def.get("geo_distance").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let origin = spec.get("origin").cloned().unwrap_or(Value::Null);
    let unit = spec.get("unit").and_then(|v| v.as_str()).unwrap_or("m");
    let scale = parse_distance(&format!("1{unit}")).unwrap_or(1.0);
    let sub_aggs = def.get("aggs").or_else(|| def.get("aggregations")).cloned();
    let keyed = spec.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);

    let probe = json!({
        "query": main_query.clone().unwrap_or_else(|| json!({"match_all": {}})),
        "size": 10_000,
        "_source": [field.clone()],
    });
    let answer = run(store, &targets.join(","), &probe, &Params::new())?;
    let path = format!("/_source/{}", field.replace('.', "/"));
    let placed: Vec<(String, f64)> = answer
        .hits
        .iter()
        .filter_map(|h| {
            let id = h.get("_id")?.as_str()?.to_string();
            let here = h.pointer(&path)?;
            let d = geo_distance_metres(&origin, here)? / scale;
            Some((id, d))
        })
        .collect();

    let mut buckets: Vec<Value> = Vec::new();
    let mut named = serde_json::Map::new();
    for range in spec.get("ranges").and_then(|v| v.as_array()).into_iter().flatten() {
        let from = range.get("from").and_then(|v| v.as_f64());
        let to = range.get("to").and_then(|v| v.as_f64());
        let ids: Vec<String> = placed
            .iter()
            .filter(|(_, d)| from.map(|f| *d >= f).unwrap_or(true))
            .filter(|(_, d)| to.map(|t| *d < t).unwrap_or(true))
            .map(|(id, _)| id.clone())
            .collect();
        let key =
            range.get("key").and_then(|k| k.as_str()).map(|s| s.to_string()).unwrap_or_else(|| {
                match (from, to) {
                    (None, Some(t)) => format!("*-{t:?}"),
                    (Some(f), None) => format!("{f:?}-*"),
                    (Some(f), Some(t)) => format!("{f:?}-{t:?}"),
                    (None, None) => "*-*".to_string(),
                }
            });
        let mut b = json!({"key": key.clone(), "doc_count": ids.len()});
        // a range with no lower edge begins at nought, and says so
        b["from"] = json!(from.unwrap_or(0.0));
        if from.is_none() && to.is_none() {
            b.as_object_mut().map(|o| o.remove("from"));
        }
        if let Some(t) = to {
            b["to"] = json!(t);
        }
        if let Some(subs) = sub_aggs.as_ref() {
            // the documents in this bucket are named outright, which is the
            // only handle a distance leaves behind
            let narrowed = json!({"bool": {"filter": [{"terms": {"_id": ids}}]}});
            let (_, sub) =
                count_with_sub_aggs(store, targets, &narrowed, &Some(subs.clone()), weighted)?;
            if let Some(Value::Object(o)) = sub {
                for (k, v) in o {
                    b[k] = v;
                }
            }
        }
        if keyed {
            named.insert(key, b);
        } else {
            buckets.push(b);
        }
    }
    if keyed {
        Ok(json!({"buckets": Value::Object(named)}))
    } else {
        Ok(json!({"buckets": buckets}))
    }
}

/// `geo_bounds` -- the smallest box that holds every point the query found.
pub(crate) fn run_geo_bounds_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("geo_bounds").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let points = points_found(store, targets, main_query, &field)?;
    if points.is_empty() {
        return Ok(json!({}));
    }
    let (mut north, mut south) = (f64::MIN, f64::MAX);
    let (mut west, mut east) = (f64::MAX, f64::MIN);
    for (lat, lon) in &points {
        north = north.max(*lat);
        south = south.min(*lat);
        west = west.min(*lon);
        east = east.max(*lon);
    }
    Ok(json!({
        "bounds": {
            "top_left": {"lat": north, "lon": west},
            "bottom_right": {"lat": south, "lon": east},
        }
    }))
}

/// `geo_centroid` -- where the points the query found sit, on average.
pub(crate) fn run_geo_centroid_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
) -> std::result::Result<Value, Response> {
    let spec = def.get("geo_centroid").cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let points = points_found(store, targets, main_query, &field)?;
    if points.is_empty() {
        return Ok(json!({"count": 0}));
    }
    let count = points.len() as f64;
    let lat = points.iter().map(|(lat, _)| lat).sum::<f64>() / count;
    let lon = points.iter().map(|(_, lon)| lon).sum::<f64>() / count;
    Ok(json!({"location": {"lat": lat, "lon": lon}, "count": points.len()}))
}

/// The points a query found, as latitude and longitude.
fn points_found(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    field: &str,
) -> std::result::Result<Vec<(f64, f64)>, Response> {
    let probe = json!({
        "query": main_query.clone().unwrap_or_else(|| json!({"match_all": {}})),
        "size": 10_000,
        "_source": [field],
    });
    let answer = run(store, &targets.join(","), &probe, &Params::new())?;
    let path = format!("/_source/{}", field.replace('.', "/"));
    Ok(answer
        .hits
        .iter()
        .filter_map(|hit| {
            let here = hit.pointer(&path)?;
            let (lat, lon) = lat_lon_of(here)?;
            Some((lat, lon))
        })
        .collect())
}

/// A point, however it was written.
fn lat_lon_of(value: &Value) -> Option<(f64, f64)> {
    match value {
        Value::Object(o) => Some((o.get("lat")?.as_f64()?, o.get("lon")?.as_f64()?)),
        // `[lon, lat]`, which is the order GeoJSON writes them in
        Value::Array(a) if a.len() == 2 => Some((a[1].as_f64()?, a[0].as_f64()?)),
        Value::String(s) => {
            let (lat, lon) = s.split_once(',')?;
            Some((lat.trim().parse().ok()?, lon.trim().parse().ok()?))
        }
        _ => None,
    }
}

/// The geohash a point sits in at a given precision.
///
/// A geohash is the latitude and longitude interleaved bit by bit, each bit
/// saying which half of the range the point is in, and the bits read off five
/// at a time as letters.
pub(crate) fn geohash_of(lat: f64, lon: f64, precision: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let (mut lat_from, mut lat_to) = (-90.0f64, 90.0f64);
    let (mut lon_from, mut lon_to) = (-180.0f64, 180.0f64);
    let mut hash = String::with_capacity(precision);
    let mut bits = 0u8;
    let mut value = 0usize;
    let mut even = true;
    while hash.len() < precision {
        match even {
            true => {
                let middle = (lon_from + lon_to) / 2.0;
                match lon > middle {
                    true => {
                        value = (value << 1) | 1;
                        lon_from = middle;
                    }
                    false => {
                        value <<= 1;
                        lon_to = middle;
                    }
                }
            }
            false => {
                let middle = (lat_from + lat_to) / 2.0;
                match lat > middle {
                    true => {
                        value = (value << 1) | 1;
                        lat_from = middle;
                    }
                    false => {
                        value <<= 1;
                        lat_to = middle;
                    }
                }
            }
        }
        even = !even;
        bits += 1;
        if bits == 5 {
            hash.push(ALPHABET[value] as char);
            bits = 0;
            value = 0;
        }
    }
    hash
}

/// The map tile a point sits in, written the way a tile server names it.
pub(crate) fn geotile_of(lat: f64, lon: f64, zoom: u32) -> String {
    let tiles = 2f64.powi(zoom as i32);
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78);
    let x = ((lon + 180.0) / 360.0 * tiles).floor().clamp(0.0, tiles - 1.0) as i64;
    let radians = lat.to_radians();
    let y = ((1.0 - (radians.tan() + 1.0 / radians.cos()).ln() / std::f64::consts::PI) / 2.0
        * tiles)
        .floor()
        .clamp(0.0, tiles - 1.0) as i64;
    format!("{zoom}/{x}/{y}")
}

/// The cell one point falls in, for whichever grid the aggregation names.
pub(crate) fn grid_key(kind: &str, lat: f64, lon: f64, precision: usize) -> String {
    match kind {
        "geotile_grid" => geotile_of(lat, lon, precision as u32),
        _ => geohash_of(lat, lon, precision),
    }
}

/// `geohash_grid` and `geotile_grid` -- how many points fall in each cell of a
/// grid laid over the world.
pub(crate) fn run_geo_grid_agg(
    store: &Store,
    targets: &[String],
    main_query: &Option<Value>,
    def: &Value,
    kind: &str,
) -> std::result::Result<Value, Response> {
    let spec = def.get(kind).cloned().unwrap_or(json!({}));
    let field = spec.get("field").and_then(|f| f.as_str()).unwrap_or("").to_string();
    let precision = spec
        .get("precision")
        .and_then(|v| v.as_u64())
        .map(|p| p as usize)
        .unwrap_or(if kind == "geotile_grid" { 7 } else { 5 });
    let size = spec.get("size").and_then(|v| v.as_u64()).unwrap_or(10_000) as usize;
    let points = points_found(store, targets, main_query, &field)?;
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    for (lat, lon) in points {
        *counts.entry(grid_key(kind, lat, lon, precision)).or_default() += 1;
    }
    // the fullest cells first, and cells holding the same number by their key
    let mut cells: Vec<(String, usize)> = counts.into_iter().collect();
    cells.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    cells.truncate(size);
    Ok(json!({
        "buckets": cells
            .into_iter()
            .map(|(key, count)| json!({"key": key, "doc_count": count}))
            .collect::<Vec<_>>(),
    }))
}

/// A shape written the way a `fields` request asked for it.
///
/// The default is the GeoJSON object the shape stands for, whatever spelling
/// it was sent in; `wkt` asks for the text form instead.
pub(crate) fn shape_as(value: &Value, format: Option<&str>) -> Option<Value> {
    let wkt = format.map(|f| f.eq_ignore_ascii_case("wkt")).unwrap_or(false);
    let (kind, coordinates) = match value {
        Value::String(text) => {
            if wkt {
                return Some(json!(text));
            }
            let (kind, rest) = text.trim().split_once('(')?;
            let kind = kind.trim().to_ascii_uppercase();
            let inside = rest.trim_end().trim_end_matches(')');
            let numbers: Vec<f64> =
                inside.split_whitespace().filter_map(|n| n.parse::<f64>().ok()).collect();
            match kind.as_str() {
                "POINT" if numbers.len() == 2 => ("Point", json!(numbers)),
                _ => return None,
            }
        }
        Value::Object(o) => {
            let kind = o.get("type")?.as_str()?.to_string();
            let coordinates = o.get("coordinates")?.clone();
            if !wkt {
                return Some(json!({"type": kind, "coordinates": coordinates}));
            }
            let numbers: Vec<f64> =
                coordinates.as_array()?.iter().filter_map(|n| n.as_f64()).collect();
            let written = numbers.iter().map(|n| format!("{n:.1}")).collect::<Vec<_>>().join(" ");
            return Some(json!(format!("{} ({written})", kind.to_ascii_uppercase())));
        }
        _ => return None,
    };
    Some(json!({"type": kind, "coordinates": coordinates}))
}
