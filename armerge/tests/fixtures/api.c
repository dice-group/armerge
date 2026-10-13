extern void *malloc(unsigned long size);
int helper_fn(int x);
int weak_fn(int x);

static int local_fn(int x) { return x * 2; }

int api_fn(int x) { return helper_fn(x) + weak_fn(x) + local_fn(x) + (malloc(1) != 0); }
