-- Receipt delivery tracking migration
-- Tracks customer receipt email delivery attempts and status

-- Enum for receipt delivery status
CREATE TYPE receipt_delivery_status AS ENUM (
    'pending',      -- Not yet attempted
    'sent',         -- Successfully sent to SMTP server
    'bounced',      -- Hard bounce (permanent delivery failure)
    'failed'        -- Other send failure (soft bounce, network error, etc.)
);

-- Receipt delivery log table
CREATE TABLE receipt_delivery_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    invoice_id VARCHAR(64) NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,

    -- Recipient email address at time of send attempt
    recipient_email VARCHAR(320) NOT NULL,

    -- Delivery status
    status receipt_delivery_status NOT NULL DEFAULT 'pending',

    -- Error details if send failed
    error_message TEXT,

    -- Timestamps
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for efficient queries
CREATE INDEX idx_receipt_delivery_invoice ON receipt_delivery_log(invoice_id);
CREATE INDEX idx_receipt_delivery_status ON receipt_delivery_log(status);
CREATE INDEX idx_receipt_delivery_pending ON receipt_delivery_log(status, attempted_at)
    WHERE status IN ('pending', 'failed');
CREATE INDEX idx_receipt_delivery_bounced ON receipt_delivery_log(status, attempted_at)
    WHERE status = 'bounced';

-- Comment for documentation
COMMENT ON TABLE receipt_delivery_log IS 'Tracks email delivery attempts for customer payment receipts';
COMMENT ON COLUMN receipt_delivery_log.status IS 'Delivery status: pending, sent, bounced, or failed';
