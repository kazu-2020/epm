-- Built-in column types only: a custom type (enum, domain) makes the client prepare a named statement to read its definition.
CREATE TABLE public.placement (
    workspace_global_id text PRIMARY KEY,
    epoch bigint NOT NULL DEFAULT 0,
    node text,
    issued_at timestamptz,
    address text,
    address_epoch bigint NOT NULL DEFAULT 0,
    recorded_at timestamptz,
    boot_duration interval,
    consecutive_failures integer NOT NULL DEFAULT 0,
    CONSTRAINT placement_counters_in_range
        CHECK (0 <= address_epoch AND address_epoch <= epoch AND consecutive_failures >= 0),
    CONSTRAINT placement_boot_duration_is_not_negative CHECK (boot_duration >= interval '0')
);

CREATE FUNCTION public.placement_reject_epoch_loss() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'placement: % must not lose an issued epoch', TG_OP
        USING ERRCODE = 'restrict_violation';
END
$$;

CREATE TRIGGER placement_epoch_never_decreases
    BEFORE UPDATE ON public.placement
    FOR EACH ROW WHEN (NEW.epoch < OLD.epoch)
    EXECUTE FUNCTION public.placement_reject_epoch_loss();

CREATE TRIGGER placement_row_is_never_deleted
    BEFORE DELETE ON public.placement
    FOR EACH ROW
    EXECUTE FUNCTION public.placement_reject_epoch_loss();

CREATE TRIGGER placement_is_never_truncated
    BEFORE TRUNCATE ON public.placement
    FOR EACH STATEMENT
    EXECUTE FUNCTION public.placement_reject_epoch_loss();
