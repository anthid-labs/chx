-- Read by query_file! in tests/macros.rs.
SELECT id, note
FROM probe
WHERE venue = {venue:String}
ORDER BY id
