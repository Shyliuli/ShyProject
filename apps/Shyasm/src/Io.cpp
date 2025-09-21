#include "Io.hpp"

namespace Io {

fn read_from_file(string path) -> Result<string, CoreError> {
    (void)path;
    return Result<string, CoreError>::Ok(string{});
}

fn write_to_file(string path, unique_ptr<Memory> memory) -> Result<Unit, CoreError> {
    (void)path;
    (void)memory;
    return Result<Unit, CoreError>::Ok();
}

} // namespace Io
