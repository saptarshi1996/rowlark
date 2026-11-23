-- Runs once when the TLS test server is first created.
CREATE ROLE rowlark_cert LOGIN;
GRANT ALL ON DATABASE rowlark_test TO rowlark_cert;
