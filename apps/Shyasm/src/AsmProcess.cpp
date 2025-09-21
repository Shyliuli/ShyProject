#include "AsmProcess.hpp"
#include <new>
#include <utility>

fn AsmProcess::create(string input, unique_ptr<Memory> memory) -> Result<unique_ptr<AsmProcess>, CoreError> {
    if (memory == nullptr) {
        return Err<unique_ptr<AsmProcess>>(CoreError(AllocError{
            .message = "AsmProcess::create received null memory"}));
    }

    auto proc = unique_ptr<AsmProcess>(new (std::nothrow) AsmProcess(std::move(input), std::move(memory)));
    if (proc == nullptr) {
        return Err<unique_ptr<AsmProcess>>(CoreError(AllocError{
            .message = "Failed to allocate AsmProcess"}));
    }

    return Ok<CoreError>(std::move(proc));
}

fn AsmProcess::process() -> Result<AsmProcess&, CoreError> {
    return Result<AsmProcess&, CoreError>::Err(CoreError{AllocError{"AsmProcess::process not implemented"}});
}

fn AsmProcess::bin() -> Result<unique_ptr<Memory>, CoreError> {
    return Result<unique_ptr<Memory>, CoreError>::Ok(unique_ptr<Memory>{});
}

AsmProcess::AsmProcess(string input, unique_ptr<Memory> memory)
    : code(std::move(input)), memory(std::move(memory)), has_processed(false) {}
