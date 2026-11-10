-- Rollback: Remove receipt delivery tracking
DROP TABLE IF EXISTS receipt_delivery_log;
DROP TYPE IF EXISTS receipt_delivery_status;
