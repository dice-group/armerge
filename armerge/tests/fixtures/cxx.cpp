// Shared "header": the inline function and template instantiation land in COMDAT groups in every TU
inline int shared_inline(int x) { return x + 1; }
template <typename T> T shared_template(T x) { return x * 2; }

namespace vendor {
int internal_detail(int x) { return shared_inline(x) + shared_template(x); }
} // namespace vendor

extern "C" int app_create(int x) { return vendor::internal_detail(x); }

extern "C" int parser_parse(int x) {
    if (x < 0) throw x;
    return shared_template(x);
}

// A destructor cleanup around a throwing call pulls in the C++ personality routine
struct Guard {
    int *p;
    ~Guard() { *p = 0; }
};
extern "C" int store_open(int *p) {
    Guard g{p};
    return parser_parse(*p);
}
