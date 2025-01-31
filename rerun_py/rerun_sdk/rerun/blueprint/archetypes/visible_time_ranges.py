# DO NOT EDIT! This file was auto-generated by crates/re_types_builder/src/codegen/python/mod.rs
# Based on "crates/re_types/definitions/rerun/blueprint/archetypes/visible_time_range.fbs".

# You can extend this class by creating a "VisibleTimeRangesExt" class in "visible_time_ranges_ext.py".

from __future__ import annotations

from typing import Any

from attrs import define, field

from ... import datatypes
from ..._baseclasses import Archetype
from ...blueprint import components as blueprint_components
from ...error_utils import catch_and_log_exceptions

__all__ = ["VisibleTimeRanges"]


@define(str=False, repr=False, init=False)
class VisibleTimeRanges(Archetype):
    """
    **Archetype**: Configures what range of each timeline is shown on a view.

    Whenever no visual time range applies, queries are done with "latest at" semantics.
    This means that the view will, starting from the time cursor position,
    query the latest data available for each component type.

    The default visual time range depends on the type of view this property applies to:
    - For time series views, the default is to show the entire timeline.
    - For any other view, the default is to apply latest-at semantics.
    """

    def __init__(self: Any, ranges: datatypes.VisibleTimeRangeArrayLike):
        """
        Create a new instance of the VisibleTimeRanges archetype.

        Parameters
        ----------
        ranges:
            The time ranges to show for each timeline unless specified otherwise on a per-entity basis.

            If a timeline is listed twice, the first entry will be used.

        """

        # You can define your own __init__ function as a member of VisibleTimeRangesExt in visible_time_ranges_ext.py
        with catch_and_log_exceptions(context=self.__class__.__name__):
            self.__attrs_init__(ranges=ranges)
            return
        self.__attrs_clear__()

    def __attrs_clear__(self) -> None:
        """Convenience method for calling `__attrs_init__` with all `None`s."""
        self.__attrs_init__(
            ranges=None,  # type: ignore[arg-type]
        )

    @classmethod
    def _clear(cls) -> VisibleTimeRanges:
        """Produce an empty VisibleTimeRanges, bypassing `__init__`."""
        inst = cls.__new__(cls)
        inst.__attrs_clear__()
        return inst

    ranges: blueprint_components.VisibleTimeRangeBatch = field(
        metadata={"component": "required"},
        converter=blueprint_components.VisibleTimeRangeBatch._required,  # type: ignore[misc]
    )
    # The time ranges to show for each timeline unless specified otherwise on a per-entity basis.
    #
    # If a timeline is listed twice, the first entry will be used.
    #
    # (Docstring intentionally commented out to hide this field from the docs)

    __str__ = Archetype.__str__
    __repr__ = Archetype.__repr__  # type: ignore[assignment]
