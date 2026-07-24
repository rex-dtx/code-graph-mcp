/// Maximum number of parameters in a single IN clause to stay within SQLite limits.
pub(super) const MAX_IN_PARAMS: usize = 500;

pub(super) fn first_row<T>(
    mut rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> rusqlite::Result<Option<T>> {
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Escape LIKE metacharacters in user-supplied input for use in a `LIKE ? ESCAPE '\'`
/// pattern. The backslash itself MUST be escaped FIRST — it is the escape char, so a
/// literal `\` in the input would otherwise consume the following char (`a\b` wrongly
/// matches `ab`, a trailing `\` matches nothing). Order is load-bearing: `\` → `%` → `_`.
pub(super) fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub(super) fn make_placeholders(start: usize, count: usize) -> String {
    (start..start + count)
        .map(|i| format!("?{}", i))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
pub(crate) fn test_db() -> (crate::storage::db::Database, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = crate::storage::db::Database::open(&tmp.path().join("test.db")).unwrap();
    (db, tmp)
}
