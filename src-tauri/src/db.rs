use rusqlite::{params, Connection, OptionalExtension};

use crate::query::to_fts_query;
use crate::tools::clean_text;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSummary {
    pub id: i64,
    pub original_name: String,
    pub status: String,
    pub error: Option<String>,
    pub note: Option<String>,
    pub page_count: i32,
    pub thumb_path: Option<String>,
    pub imported_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub document_id: i64,
    pub page_number: i32,
    pub original_name: String,
    pub snippet: String,
    pub thumb_path: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageDetail {
    pub page_number: i32,
    pub image_path: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDetail {
    pub id: i64,
    pub original_name: String,
    pub stored_path: String,
    pub status: String,
    pub error: Option<String>,
    pub note: Option<String>,
    pub pages: Vec<PageDetail>,
}

#[derive(Debug, Clone)]
pub struct NewPage {
    pub page_number: i32,
    pub image_path: Option<String>,
    pub text: String,
}

pub fn open(path: &std::path::Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|error| error.to_string())?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;

        CREATE TABLE IF NOT EXISTS documents (
            id INTEGER PRIMARY KEY,
            original_name TEXT NOT NULL,
            stored_path TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL,
            error TEXT,
            note TEXT,
            page_count INTEGER NOT NULL DEFAULT 0,
            thumb_path TEXT,
            imported_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS pages (
            id INTEGER PRIMARY KEY,
            document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
            page_number INTEGER NOT NULL,
            image_path TEXT,
            text TEXT NOT NULL DEFAULT ''
        );

        CREATE INDEX IF NOT EXISTS idx_pages_document ON pages(document_id);

        CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
            title,
            body,
            tokenize = \"unicode61 tokenchars '.' remove_diacritics 2\"
        );

        CREATE TRIGGER IF NOT EXISTS pages_ai AFTER INSERT ON pages BEGIN
            INSERT INTO pages_fts(rowid, title, body)
            VALUES (
                new.id,
                (SELECT original_name FROM documents WHERE id = new.document_id),
                new.text
            );
        END;

        CREATE TRIGGER IF NOT EXISTS pages_ad AFTER DELETE ON pages BEGIN
            DELETE FROM pages_fts WHERE rowid = old.id;
        END;
        ",
    )
    .map_err(|error| error.to_string())
}

pub fn insert_document(conn: &Connection, original_name: &str) -> Result<i64, String> {
    let imported_at = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO documents (original_name, stored_path, status, imported_at)
         VALUES (?1, '', 'reading', ?2)",
        params![original_name, imported_at],
    )
    .map_err(|error| error.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub fn set_stored_path(conn: &Connection, id: i64, stored_path: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE documents SET stored_path = ?1 WHERE id = ?2",
        params![stored_path, id],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn finish_document(
    conn: &Connection,
    id: i64,
    status: &str,
    error: Option<&str>,
    note: Option<&str>,
    thumb_path: Option<&str>,
    pages: &[NewPage],
) -> Result<(), String> {
    let tx = conn.unchecked_transaction().map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM pages WHERE document_id = ?1", params![id])
        .map_err(|error| error.to_string())?;
    for page in pages {
        tx.execute(
            "INSERT INTO pages (document_id, page_number, image_path, text)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, page.page_number, page.image_path, page.text],
        )
        .map_err(|error| error.to_string())?;
    }
    tx.execute(
        "UPDATE documents
         SET status = ?1, error = ?2, note = ?3, page_count = ?4, thumb_path = ?5
         WHERE id = ?6",
        params![status, error, note, pages.len() as i32, thumb_path, id],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

pub fn mark_error(conn: &Connection, id: i64, message: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE documents SET status = 'error', error = ?1, page_count = 0 WHERE id = ?2",
        params![message, id],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn list_documents(conn: &Connection) -> Result<Vec<DocumentSummary>, String> {
    let mut statement = conn
        .prepare(
            "SELECT id, original_name, status, error, note, page_count, thumb_path, imported_at
             FROM documents
             ORDER BY imported_at DESC, id DESC",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], map_summary)
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

pub fn get_summary(conn: &Connection, id: i64) -> Result<DocumentSummary, String> {
    conn.query_row(
        "SELECT id, original_name, status, error, note, page_count, thumb_path, imported_at
         FROM documents WHERE id = ?1",
        params![id],
        map_summary,
    )
    .map_err(|error| error.to_string())
}

pub fn ready_names(conn: &Connection) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare("SELECT original_name FROM documents WHERE status = 'ready'")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

pub fn search(conn: &Connection, raw_query: &str) -> Result<Vec<SearchHit>, String> {
    let Some(match_query) = to_fts_query(raw_query) else {
        return Ok(Vec::new());
    };
    let mut statement = conn
        .prepare(
            "SELECT pages.document_id,
                    pages.page_number,
                    documents.original_name,
                    snippet(pages_fts, -1, char(1), char(2), '…', 18),
                    documents.thumb_path
             FROM pages_fts
             JOIN pages ON pages.id = pages_fts.rowid
             JOIN documents ON documents.id = pages.document_id
             WHERE pages_fts MATCH ?1
             ORDER BY bm25(pages_fts)
             LIMIT 50",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![match_query], |row| {
            Ok(SearchHit {
                document_id: row.get(0)?,
                page_number: row.get(1)?,
                original_name: row.get(2)?,
                snippet: clean_text(&row.get::<_, String>(3)?),
                thumb_path: row.get(4)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

pub fn get_document(conn: &Connection, id: i64) -> Result<Option<DocumentDetail>, String> {
    let summary = conn
        .query_row(
            "SELECT id, original_name, stored_path, status, error, note
             FROM documents WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;

    let Some((doc_id, original_name, stored_path, status, error, note)) = summary else {
        return Ok(None);
    };

    let mut statement = conn
        .prepare(
            "SELECT page_number, image_path, text
             FROM pages
             WHERE document_id = ?1
             ORDER BY page_number",
        )
        .map_err(|error| error.to_string())?;
    let pages = statement
        .query_map(params![id], |row| {
            Ok(PageDetail {
                page_number: row.get(0)?,
                image_path: row.get(1)?,
                text: clean_text(&row.get::<_, String>(2)?),
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;

    Ok(Some(DocumentDetail {
        id: doc_id,
        original_name,
        stored_path,
        status,
        error,
        note,
        pages,
    }))
}

fn map_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentSummary> {
    Ok(DocumentSummary {
        id: row.get(0)?,
        original_name: row.get(1)?,
        status: row.get(2)?,
        error: row.get(3)?,
        note: row.get(4)?,
        page_count: row.get(5)?,
        thumb_path: row.get(6)?,
        imported_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().expect("memory db");
        migrate(&conn).expect("migrate");
        conn
    }

    fn add_page(conn: &Connection, name: &str, text: &str) -> i64 {
        let id = insert_document(conn, name).unwrap();
        finish_document(
            conn,
            id,
            "ready",
            None,
            None,
            None,
            &[NewPage {
                page_number: 1,
                image_path: None,
                text: text.to_string(),
            }],
        )
        .unwrap();
        id
    }

    #[test]
    fn finds_warranty_with_highlights() {
        let conn = memory_db();
        let id = add_page(
            &conn,
            "warranty-dishwasher.png",
            "This dishwasher warranty covers parts and labor for five years.",
        );
        let hits = search(&conn, "dishwasher warranty").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].document_id, id);
        assert!(hits[0].snippet.contains('\u{1}'));
        assert!(hits[0].snippet.to_lowercase().contains("warranty"));
    }

    #[test]
    fn amount_with_decimal_is_one_token() {
        let conn = memory_db();
        add_page(&conn, "receipt.png", "TOTAL 42.99\nCard payment.");
        let hits = search(&conn, "total 42.99").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(search(&conn, "42.98").unwrap().is_empty());
    }

    #[test]
    fn deleting_pages_removes_them_from_search() {
        let conn = memory_db();
        let id = add_page(&conn, "letter.pdf", "Mara Ellison will return the ladder.");
        assert_eq!(search(&conn, "Mara Ellison").unwrap().len(), 1);
        finish_document(&conn, id, "ready", None, None, None, &[]).unwrap();
        assert!(search(&conn, "Mara Ellison").unwrap().is_empty());
    }
}
