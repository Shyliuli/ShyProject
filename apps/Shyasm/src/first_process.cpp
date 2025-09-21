#include "FirstProcess.hpp"
#include <new>
#include <utility>

fn FirstProcess::create(string input) -> Result<unique_ptr<FirstProcess>, CoreError> {
    auto process = unique_ptr<FirstProcess>(new (std::nothrow) FirstProcess(std::move(input)));
    if (process == nullptr) {
        return Err<unique_ptr<FirstProcess>>(CoreError(AllocError{
            .message = "Failed to allocate FirstProcess"}));
    }
    return Ok<CoreError>(std::move(process));
}

fn FirstProcess::comment_process() -> FirstProcess& {
    return *this;
}

fn FirstProcess::macro_process() -> Result<FirstProcess&, CoreError> {
    return Result<FirstProcess&, CoreError>::Ok(*this);
}

fn FirstProcess::flag_process() -> Result<FirstProcess&, CoreError> {
    return Result<FirstProcess&, CoreError>::Ok(*this);
}

fn FirstProcess::to_string() -> string {
    return string{};
}

FirstProcess::FirstProcess(string input)
    : code(std::move(input)) {}
