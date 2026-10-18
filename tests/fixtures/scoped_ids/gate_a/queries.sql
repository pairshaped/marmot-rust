-- func: gate_a
-- produces: id = scope_home::scoped_ids::ScopedOrderId
SELECT id, status FROM orders
WHERE org_id = @org_id AND user_id = @user_id AND id = @order_id AND status IN ('paid', 'submitted');
