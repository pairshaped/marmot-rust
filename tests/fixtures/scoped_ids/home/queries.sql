-- func: list_children
-- consumes: order_id = scope_home::scoped_ids::ScopedOrderId
SELECT id, name FROM children WHERE order_id = @order_id AND user_id = @user_id;
