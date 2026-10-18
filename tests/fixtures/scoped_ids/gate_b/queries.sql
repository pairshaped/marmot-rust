-- func: gate_b
-- produces: id = scope_home::scoped_ids::ScopedOrderId
SELECT id, status FROM orders
WHERE user_id = @user_id AND id = @order_id AND org_id = @org_id AND status IN ('paid', 'submitted');
