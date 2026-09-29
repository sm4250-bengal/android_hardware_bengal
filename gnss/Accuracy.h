/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include "position.h"
#include <cmath>
#include <optional>

namespace bengal::gnss {
// Native LOC's circular radius convention is63% (pinned LocApiV02.cpp:4885).
// 1.072 is its conservative63→68% scale. Ellipse confidence belongs to the
// ellipse even when a circle is present; never use it to shrink the circle.
inline std::optional<double> horizontalAccuracy68(const loc_fix& f) {
    double accuracy;
    if (f.fields & LOC_FIX_HORIZONTAL_CIRCLE) {
        accuracy = static_cast<double>(f.horizontal_circle_m) * 1.072;
    } else if ((f.fields & (LOC_FIX_HORIZONTAL_ELLIPSE | LOC_FIX_HORIZONTAL_CONFIDENCE)) ==
               (LOC_FIX_HORIZONTAL_ELLIPSE | LOC_FIX_HORIZONTAL_CONFIDENCE) &&
               f.horizontal_confidence > 0 && f.horizontal_confidence <= 99 &&
               std::isfinite(f.horizontal_minor_m) && f.horizontal_minor_m > 0 &&
               f.horizontal_major_m >= f.horizontal_minor_m) {
        // The major-axis circle encloses the confidence ellipse. Under the
        // same2D Gaussian model, scale up to68%; never shrink >=68% bounds.
        double scale = f.horizontal_confidence >= 68 ? 1.0 :
            std::sqrt(std::log(.32) / std::log1p(-f.horizontal_confidence / 100.0));
        accuracy = f.horizontal_major_m * scale;
    } else return std::nullopt;
    // Framework converts accuracy tofloat; reject overflow, not just double INF.
    if (!std::isfinite(accuracy) || accuracy <= 0 || !std::isfinite(static_cast<float>(accuracy)))
        return std::nullopt;
    return accuracy;
}
inline std::optional<double> verticalAccuracy68(const loc_fix& f) {
    if ((f.fields & (LOC_FIX_ALTITUDE | LOC_FIX_VERTICAL_UNCERTAINTY | LOC_FIX_VERTICAL_CONFIDENCE)) !=
        (LOC_FIX_ALTITUDE | LOC_FIX_VERTICAL_UNCERTAINTY | LOC_FIX_VERTICAL_CONFIDENCE) ||
        f.vertical_confidence < 68 || f.vertical_confidence > 99 ||
        !std::isfinite(f.vertical_uncertainty_m) || f.vertical_uncertainty_m <= 0)
        return std::nullopt;
    return f.vertical_uncertainty_m;
}
} // namespace bengal::gnss
