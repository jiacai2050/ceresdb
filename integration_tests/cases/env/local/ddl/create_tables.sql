DROP TABLE IF EXISTS `05_create_tables_t`;
DROP TABLE IF EXISTS `05_create_tables_t2`;
DROP TABLE IF EXISTS `05_create_tables_t3`;
DROP TABLE IF EXISTS `05_create_tables_t4`;
DROP TABLE IF EXISTS `05_create_tables_t5`;
DROP TABLE IF EXISTS `05_create_tables_t6`;
DROP TABLE IF EXISTS `05_create_tables_t7`;
DROP TABLE IF EXISTS `05_create_tables_t8`;
DROP TABLE IF EXISTS `05_create_tables_t9`;

-- no TIMESTAMP column
CREATE TABLE `05_create_tables_t`(c1 int) ENGINE = Analytic;

CREATE TABLE `05_create_tables_t`(c1 int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;

CREATE TABLE IF NOT EXISTS `05_create_tables_t`(c1 int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;

-- table already exist
CREATE TABLE `05_create_tables_t`(c1 int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;

create table `05_create_tables_t2`(a int, b int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic with (enable_ttl='false');
insert into `05_create_tables_t2`(a, b, t) values(1,1,1),(2,2,2);
select a+b from `05_create_tables_t2`;

-- table already exist
create table `05_create_tables_t2`(a int,b int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;
-- table already exist
create table `05_create_tables_t2`(a int,b int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;

create table `05_create_tables_t3`(a int,b int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;

create table `05_create_tables_t4`(`a` int, t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;
describe table `05_create_tables_t4`;
show create table `05_create_tables_t4`;

-- TIMESTAMP KEY
CREATE TABLE `05_create_tables_t5`(c1 int, t timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic;
describe table `05_create_tables_t5`;
show create table `05_create_tables_t5`;

-- Multiple TIMESTAMP KEYs
CREATE TABLE `05_create_tables_t6`(c1 int, t1 timestamp NOT NULL TIMESTAMP KEY, t2 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic;

-- Column with comment
CREATE TABLE `05_create_tables_t7`(c1 int COMMENT 'id', t timestamp NOT NULL, TIMESTAMP KEY(t)) ENGINE = Analytic;
describe table `05_create_tables_t7`;
show create table `05_create_tables_t7`;

-- StorageFormat
CREATE TABLE `05_create_tables_t8`(c1 int, t1 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic;
show create table `05_create_tables_t8`;
drop table `05_create_tables_t8`;

CREATE TABLE `05_create_tables_t8`(c1 int, t1 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic with (storage_format= 'columnar');
show create table `05_create_tables_t8`;
drop table `05_create_tables_t8`;

CREATE TABLE `05_create_tables_t9`(c1 int, d string dictionary, t1 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic with (storage_format= 'columnar');
show create table `05_create_tables_t9`;
drop table `05_create_tables_t9`;

CREATE TABLE `05_create_tables_t9`(c1 int, d string dictionary, t1 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic;
show create table `05_create_tables_t9`;
drop table `05_create_tables_t9`;

-- Error: dictionary must be string type
CREATE TABLE `05_create_tables_t9`(c1 int, d double dictionary, t1 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic;

-- Ignore now, table_id is not stable now
-- CREATE TABLE `05_create_tables_t8`(c1 int, t1 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic with (storage_format= 'unknown');

-- Default value options
CREATE TABLE `05_create_tables_t9`(c1 int, c2 bigint default 0, c3 uint32 default 1 + 1, c4 string default 'xxx', c5 uint32 default c3*2 + 1, t1 timestamp NOT NULL TIMESTAMP KEY) ENGINE = Analytic;
show create table `05_create_tables_t9`;
drop table `05_create_tables_t9`;

-- Explicit primary key with tsid
CREATE TABLE `05_create_tables_t10`(c1 int, t1 timestamp NOT NULL TIMESTAMP KEY, PRIMARY KEY(tsid, t1)) ENGINE = Analytic;
show create table `05_create_tables_t10`;
drop table `05_create_tables_t10`;

-- Explicit primary key with tsid
CREATE TABLE `05_create_tables_t11`(c1 int, t1 timestamp NOT NULL TIMESTAMP KEY, PRIMARY KEY(t1, tsid)) ENGINE = Analytic;
show create table `05_create_tables_t11`;
drop table `05_create_tables_t11`;

-- Timestamp not in primary key
CREATE TABLE `05_timestamp_not_in_primary_key`(c1 int NOT NULL, t timestamp NOT NULL, TIMESTAMP KEY(t), PRIMARY KEY(c1)) ENGINE = Analytic;

DROP TABLE IF EXISTS `05_create_tables_t`;
DROP TABLE IF EXISTS `05_create_tables_t2`;
DROP TABLE IF EXISTS `05_create_tables_t3`;
DROP TABLE IF EXISTS `05_create_tables_t4`;
DROP TABLE IF EXISTS `05_create_tables_t5`;
DROP TABLE IF EXISTS `05_create_tables_t6`;
DROP TABLE IF EXISTS `05_create_tables_t7`;
DROP TABLE IF EXISTS `05_create_tables_t8`;
DROP TABLE IF EXISTS `05_create_tables_t9`;
DROP TABLE IF EXISTS `05_create_tables_t10`;
DROP TABLE IF EXISTS `05_create_tables_t11`;
DROP TABLE IF EXISTS `05_create_tables_t12`;
