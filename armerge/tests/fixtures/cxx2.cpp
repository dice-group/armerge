// Same "header" as cxx.cpp, so both objects define the same COMDAT symbols
inline int shared_inline(int x) { return x + 1; }
template <typename T> T shared_template(T x) { return x * 2; }

extern "C" int store_size(int x) { return shared_inline(x) - shared_template(x); }
