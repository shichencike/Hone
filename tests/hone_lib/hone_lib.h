// hone_lib.h - 测试头文件（配合 tests/hone_lib 动态库，验证 from "header.h" 自动绑定）
// 导出函数与 tests/hone_lib/src/lib.rs 一致；末尾附加不支持原型用于测试解析器标记。

typedef long long int hone_int64;
typedef unsigned long hone_size_t;
typedef struct hone_handle hone_handle;

// 标量类型
hone_int64 lib_add(hone_int64 a, hone_int64 b);
hone_int64 lib_mul(hone_int64 a, hone_int64 b);
hone_int64 lib_fact(hone_int64 n);
hone_int64 lib_echo(hone_int64 x);

// 浮点
double lib_add_f(double a, double b);
double lib_mix_f(double f, hone_int64 n);

// 字符串
hone_int64 lib_strlen(const char* s);
const char* lib_hello(void);
hone_int64 lib_count_char(const char* s, hone_int64 c);

// 布尔（头文件为 int，映射到 int）
hone_int64 lib_not(hone_int64 b);

// 指针
void* lib_echo_ptr(void* p);

// 多参数
hone_int64 lib_sum4(hone_int64 a, hone_int64 b, hone_int64 c, hone_int64 d);

// void 返回 + 全局状态
void lib_bump(void);
hone_int64 lib_count(void);

// ---- 以下原型不受支持，解析器会标记 unsupported（hone bind 输出为注释）----

hone_int64 lib_cb(int (*cb)(void*));      // 回调
hone_int64 lib_var(const char* fmt, ...); // 变参
hone_int64 lib_arr(int a[4]);             // 数组参数
struct hone_handle lib_mk(void);          // 结构体按值返回
