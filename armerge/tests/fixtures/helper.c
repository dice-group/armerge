int helper_data = 42;

int helper_fn(int x) { return x + helper_data; }

__attribute__((weak)) int weak_fn(int x) { return x - 1; }
