-- RFC-v0.7 Wave 0 placeholder migration.
--
-- `sqlx::migrate!` refuses to compile without at least one file in
-- the migrations directory. This placeholder establishes the
-- migrations machinery so `ff_backend_postgres::migrate::apply_migrations`
-- has something to apply from tests, and so future waves can simply
-- drop in `0002_*.sql` etc. Wave 3 replaces this with the real
-- schema (tables, indexes, partitions per Q5, and the
-- `backward_compatible` annotation column per Q12).
SELECT 1;
