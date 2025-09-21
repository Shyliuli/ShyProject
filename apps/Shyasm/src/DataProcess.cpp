#include "DataProcess.hpp"
#include <new>
#include <utility>

fn DataProcess::create(string input) -> Result<unique_ptr<DataProcess>, CoreError> {
    auto process = unique_ptr<DataProcess>(new (std::nothrow) DataProcess(std::move(input)));
    if (process == nullptr) {
        return Err<unique_ptr<DataProcess>>(CoreError(AllocError{
            .message = "Failed to allocate DataProcess"}));
    }
    return Ok<CoreError>(std::move(process));
}

fn DataProcess::process() -> Result<DataProcess&, CoreError> {
    return Result<DataProcess&, CoreError>::Err(CoreError{AllocError{"DataProcess::process not implemented"}});
}

fn DataProcess::bin() -> Result<unique_ptr<Memory>, CoreError> {
    return Result<unique_ptr<Memory>, CoreError>::Ok(unique_ptr<Memory>{});
}

DataProcess::DataProcess(string input)
    : code(std::move(input)), memory(nullptr), has_processed(false) {}
