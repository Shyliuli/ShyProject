#include "ShyISA/command.hpp"

// | 16 进制地址                | 命令名称    | 命令格式                        | 描述                                                                                                                             |
// | :------------------------- | :---------- | :------------------------------ | :------------------------------------------------------------------------------------------------------------------------------- |
// | **算术运算指令**     |             |                                 |                                                                                                                                  |
// | `0x20`                   | `adda`    | `adda <address> <address>`    | 将两个地址中的值相加，结果存储在第一个地址中。                                                                                   |
// | `0x21`                   | `addn`    | `addn <address> <number>`     | 将一个地址中的值与一个数字相加，结果存储在该地址中。                                                                             |
// | `0x22`                   | `suba`    | `suba <address> <address>`    | 将第一个地址中的值减去第二个地址中的值，结果存储在第一个地址中。                                                                 |
// | `0x23`                   | `subn`    | `subn <address> <number>`     | 将一个地址中的值减去一个数字，结果存储在该地址中。                                                                               |
// | `0x24`                   | `mula`    | `mula <address> <address>`    | 将两个地址中的值相乘，结果存储在第一个地址中。                                                                                   |
// | `0x25`                   | `muln`    | `muln <address> <number>`     | 将一个地址中的值与一个数字相乘，结果存储在该地址中。                                                                             |
// | `0x26`                   | `diva`    | `diva <address> <address>`    | 将第一个地址中的值除以第二个地址中的值，结果存储在第一个地址中。无符号除法，除以0时结果为 `0xFFFFFFFF`。                       |
// | `0x27`                   | `divn`    | `divn <address> <number>`     | 将一个地址中的值除以一个数字，结果存储在该地址中。无符号除法，除以0时结果为 `0xFFFFFFFF`。                                     |
// | **位运算指令**       |             |                                 |                                                                                                                                  |
// | `0x28`                   | `lsa`     | `lsa <address> <address>`     | 将第一个地址中的值左移第二个地址中的值位，结果存储在第一个地址中。                                                               |
// | `0x29`                   | `lsn`     | `lsn <address> <number>`      | 将一个地址中的值左移一定位数，结果存储在该地址中。                                                                               |
// | `0x2A`                   | `rsa`     | `rsa <address> <address>`     | 将第一个地址中的值右移第二个地址中的值位，结果存储在第一个地址中。                                                               |
// | `0x2B`                   | `rsn`     | `rsn <address> <number>`      | 将一个地址中的值右移一定位数，结果存储在该地址中。                                                                               |
// | `0x2C`                   | `anda`    | `anda <address> <address>`    | 将两个地址中的值进行与运算，结果存储在第一个地址中。                                                                             |
// | `0x2D`                   | `andn`    | `andn <address> <number>`     | 将一个地址中的值与一个数字进行与运算，结果存储在该地址中。                                                                       |
// | `0x2E`                   | `ora`     | `ora <address> <address>`     | 将两个地址中的值进行或运算，结果存储在第一个地址中。                                                                             |
// | `0x2F`                   | `orn`     | `orn <address> <number>`      | 将一个地址中的值与一个数字进行或运算，结果存储在该地址中。                                                                       |
// | `0x30`                   | `xora`    | `xora <address> <address>`    | 将两个地址中的值进行异或运算，结果存储在第一个地址中。                                                                           |
// | `0x31`                   | `xorn`    | `xorn <address> <number>`     | 将一个地址中的值与一个数字进行异或运算，结果存储在该地址中。                                                                     |
// | `0x32`                   | `nota`    | `nota <address>`              | 对一个地址中的值进行非运算，结果存储在该地址中。                                                                                 |
// | **比较指令**         |             |                                 |                                                                                                                                  |
// | `0x33`                   | `equa`    | `equa <address> <address>`    | 比较两个地址中的值是否相等。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                                       |
// | `0x34`                   | `equn`    | `equn <address> <number>`     | 比较一个地址中的值是否等于一个数字。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                               |
// | `0x35`                   | `biga`    | `biga <address> <address>`    | 比较第一个地址中的值是否大于第二个地址中的值。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                     |
// | `0x36`                   | `bign`    | `bign <address> <number>`     | 比较一个地址中的值是否大于一个数字。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                               |
// | `0x37`                   | `bigequa` | `bigequa <address> <address>` | 比较第一个地址中的值是否大于等于第二个地址中的值。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                 |
// | `0x38`                   | `bigequn` | `bigequn <address> <number>`  | 比较一个地址中的值是否大于等于一个数字。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                           |
// | `0x39`                   | `smaa`    | `smaa <address> <address>`    | 比较第一个地址中的值是否小于第二个地址中的值。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                     |
// | `0x3A`                   | `sman`    | `sman <address> <number>`     | 比较一个地址中的值是否小于一个数字。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                               |
// | `0x3B`                   | `smaequa` | `smaequa <address> <address>` | 比较第一个地址中的值是否小于等于第二个地址中的值。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                 |
// | `0x3C`                   | `smaequn` | `smaequn <address> <number>`  | 比较一个地址中的值是否小于等于一个数字。条件成立时 `rs` 置 `1`，不成立时置 `0`。                                           |
// | **内存直接操作指令** |             |                                 |                                                                                                                                  |
// | `0x3D`                   | `seta`    | `seta <address> <address>`    | 将第二个地址中的值赋给第一个地址。                                                                                               |
// | `0x3E`                   | `setn`    | `setn <address> <number>`     | 将一个数字赋给指定地址中的值。                                                                                                   |
// | **内存间接操作指令** |             |                                 |                                                                                                                                  |
// | `0x3F`                   | `geta`    | `geta <address1> <address2>`  | 取地址操作，将 `<address2>` 所存的值视作地址，取其指向的值放到 `address1` 的区域。                                           |
// | `0x40`                   | `getn`    | `getn <address> <number>`     | 取地址操作，将 `number` 视作地址，取其指向的值放到 `address` 的区域。实际效果相当于 `seta`。                               |
// | `0x41`                   | `puta`    | `puta <address1> <address2>`  | 存地址操作，将 `<address2>` 所存的值放入 `<address1>` 所存的值视作地址指向的区域。                                           |
// | `0x42`                   | `putn`    | `putn <address> <number>`     | 存地址操作，将 `number` 放入 `<address>` 所存的值视作地址指向的区域。                                                        |
// | **栈操作指令**       |             |                                 |                                                                                                                                  |
// | `0x43`                   | `pusha`   | `pusha <address>`             | 将地址中的值压入栈，`sp` 自减 `1`。                                                                                          |
// | `0x44`                   | `pushn`   | `pushn <number>`              | 将立即数压入栈，`sp` 自减 `1`。                                                                                              |
// | `0x45`                   | `popa`    | `popa <address>`              | 从栈弹出值到指定地址，`sp` 自增 `1`。                                                                                        |
// | `0x46`                   | `pop`     | `pop`                         | 从栈弹出值（丢弃），`sp` 自增 `1`。                                                                                          |
// | **控制流指令**       |             |                                 |                                                                                                                                  |
// | `0x47`                   | `jmpa`    | `jmpa <address>`              | 如果 `rs` 为 `1`，跳转到指定地址所存标签处，并将 `rs` 设置为 `0`。                                                       |
// | `0x48`                   | `jmpn`    | `jmpn <label>`                | 如果 `rs` 为 `1`，跳转到指定标签处继续运行，并将 `rs` 设置为 `0`。                                                       |
// | `0x49`                   | `ujmpa`   | `ujmpa <address>`             | 无条件跳转到地址中存储的位置。                                                                                                   |
// | `0x4A`                   | `ujmpn`   | `ujmpn <label>`               | 无条件跳转到指定标签/地址。                                                                                                      |
// | `0x4B`                   | `calla`   | `calla <address>`             | 调用地址中存储的函数地址，将返回地址 (`PC+3`) 压栈。                                                                           |
// | `0x4C`                   | `calln`   | `calln <label>`               | 调用指定标签/地址的函数，将返回地址 (`PC+3`) 压栈。                                                                            |
// | `0x4D`                   | `ret`     | `ret`                         | 从栈弹出返回地址并跳转。                                                                                                         |
// | **I/O 指令**         |             |                                 |                                                                                                                                  |
// | `0x4E`                   | `ina`     | `ina <address>`               | 从标准输入读取值到指定地址。                                                                                                     |
// | `0x4F`                   | `inaasc`  | `inaasc <address>`            | 从标准输入按照 ASCII 读取值到指定地址。                                                                                          |
// | `0x50`                   | `outa`    | `outa <address>`              | 输出指定地址中的值。                                                                                                             |
// | `0x51`                   | `outn`    | `outn <number>`               | 直接输出一个数字。                                                                                                               |
// | `0x52`                   | `outaasc` | `outaasc <address>`           | 以 ASCII 字符形式输出地址中的值（仅当值在有效 ASCII 范围内）。                                                                   |
// | `0x53`                   | `outnasc` | `outnasc <number>`            | 以 ASCII 字符形式直接输出数字（仅当数字在有效 ASCII 范围内）。                                                                   |
// | **特殊指令**         |             |                                 |                                                                                                                                  |
// | `0x54`                   | `blta`    | `blta <address>`              | 内存块复制操作，需配合 `blts`（源地址）和 `bltl`（长度）指令，将 `blts` 存储的区域后 `bltl` 长的空间复制到 `address`。 |

Command::Command(u32 id):command_id(id){}
std::unordered_map<std::string,Command> Command::command_map={
    {"adda", Command(0x20)},
    {"addn", Command(0x21)},
    {"suba", Command(0x22)},
    {"subn", Command(0x23)},
    {"mula", Command(0x24)},
    {"muln", Command(0x25)},
    {"diva", Command(0x26)},
    {"divn", Command(0x27)},
    {"lsa", Command(0x28)},
    {"lsn", Command(0x29)},
    {"rsa", Command(0x2A)},
    {"rsn", Command(0x2B)},
    {"anda", Command(0x2C)},
    {"andn", Command(0x2D)},
    {"ora", Command(0x2E)},
    {"orn", Command(0x2F)},
    {"xora", Command(0x30)},
    {"xorn", Command(0x31)},
    {"nota", Command(0x32)},
    {"equa", Command(0x33)},
    {"equn", Command(0x34)},
    {"biga", Command(0x35)},
    {"bign", Command(0x36)},
    {"bigequa", Command(0x37)},
    {"bigequn", Command(0x38)},
    {"smaa", Command(0x39)},
    {"sman", Command(0x3A)},
    {"smaequa", Command(0x3B)},
    {"smaequn", Command(0x3C)},
    {"seta", Command(0x3D)},
    {"setn", Command(0x3E)},
    {"geta", Command(0x3F)},
    {"getn", Command(0x40)},
    {"puta", Command(0x41)},
    {"putn", Command(0x42)},
    {"pusha", Command(0x43)},
    {"pushn", Command(0x44)},
    {"popa", Command(0x45)},
    {"pop", Command(0x46)},
    {"jmpa", Command(0x47)},
    {"jmpn", Command(0x48)},
    {"ujmpa", Command(0x49)},
    {"ujmpn", Command(0x4A)},
    {"calla", Command(0x4B)},
    {"calln", Command(0x4C)},
    {"ret", Command(0x4D)},
    {"ina", Command(0x4E)},
    {"inaasc", Command(0x4F)},
    {"outa", Command(0x50)},
    {"outn", Command(0x51)},
    {"outaasc", Command(0x52)},
    {"outnasc", Command(0x53)},
    {"blta", Command(0x54)}
};
fn Command::str_2_command(const std::string& str)->Result<Command,CoreError>{
    auto it = command_map.find(str);
    if(it != command_map.end()) {
        return Ok<CoreError>(it->second);
    } else {
        return Err<Command>(CoreError{InvalidType{.message = "Invalid command", .type = str}});
    }
}
