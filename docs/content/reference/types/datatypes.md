---
title: "Datatypes"
order: 3
---

<!-- DO NOT EDIT! This file was auto-generated by crates/re_types_builder/src/codegen/docs/mod.rs -->

Data types are the lowest layer of the data model hierarchy. They are re-usable types used by the components.


* [`Angle`](datatypes/angle.md): Angle in either radians or degrees.
* [`AnnotationInfo`](datatypes/annotation_info.md): Annotation info annotating a class id or key-point id.
* [`Bool`](datatypes/bool.md): A single boolean.
* [`ClassDescription`](datatypes/class_description.md): The description of a semantic Class.
* [`ClassDescriptionMapElem`](datatypes/class_description_map_elem.md): A helper type for mapping class IDs to class descriptions.
* [`ClassId`](datatypes/class_id.md): A 16-bit ID representing a type of semantic class.
* [`EntityPath`](datatypes/entity_path.md): A path to an entity in the `DataStore`.
* [`Float32`](datatypes/float32.md): A single-precision 32-bit IEEE 754 floating point number.
* [`KeypointId`](datatypes/keypoint_id.md): A 16-bit ID representing a type of semantic keypoint within a class.
* [`KeypointPair`](datatypes/keypoint_pair.md): A connection between two `Keypoints`.
* [`Mat3x3`](datatypes/mat3x3.md): A 3x3 Matrix.
* [`Mat4x4`](datatypes/mat4x4.md): A 4x4 Matrix.
* [`Material`](datatypes/material.md): Material properties of a mesh.
* [`Quaternion`](datatypes/quaternion.md): A Quaternion represented by 4 real numbers.
* [`Range1D`](datatypes/range1d.md): A 1D range, specifying a lower and upper bound.
* [`Range2D`](datatypes/range2d.md): An Axis-Aligned Bounding Box in 2D space, implemented as the minimum and maximum corners.
* [`Rgba32`](datatypes/rgba32.md): An RGBA color with unmultiplied/separate alpha, in sRGB gamma space with linear alpha.
* [`Rotation3D`](datatypes/rotation3d.md): A 3D rotation.
* [`RotationAxisAngle`](datatypes/rotation_axis_angle.md): 3D rotation represented by a rotation around a given axis.
* [`Scale3D`](datatypes/scale3d.md): 3D scaling factor, part of a transform representation.
* [`TensorBuffer`](datatypes/tensor_buffer.md): The underlying storage for a `Tensor`.
* [`TensorData`](datatypes/tensor_data.md): A multi-dimensional `Tensor` of data.
* [`TensorDimension`](datatypes/tensor_dimension.md): A single dimension within a multi-dimensional tensor.
* [`TimeInt`](datatypes/time_int.md): A 64-bit number describing either nanoseconds OR sequence numbers.
* [`TimeRange`](datatypes/time_range.md): Visible time range bounds for a specific timeline.
* [`TimeRangeBoundary`](datatypes/time_range_boundary.md): Type of boundary for visible history.
* [`TimeRangeBoundaryKind`](datatypes/time_range_boundary_kind.md): Kind of boundary for visible history, see `TimeRangeBoundary`.
* [`Transform3D`](datatypes/transform3d.md): Representation of a 3D affine transform.
* [`TranslationAndMat3x3`](datatypes/translation_and_mat3x3.md): Representation of an affine transform via a 3x3 affine matrix paired with a translation.
* [`TranslationRotationScale3D`](datatypes/translation_rotation_scale3d.md): Representation of an affine transform via separate translation, rotation & scale.
* [`UInt32`](datatypes/uint32.md): A 32bit unsigned integer.
* [`UInt64`](datatypes/uint64.md): A 64bit unsigned integer.
* [`UVec2D`](datatypes/uvec2d.md): A uint32 vector in 2D space.
* [`UVec3D`](datatypes/uvec3d.md): A uint32 vector in 3D space.
* [`UVec4D`](datatypes/uvec4d.md): A uint vector in 4D space.
* [`Utf8`](datatypes/utf8.md): A string of text, encoded as UTF-8.
* [`Uuid`](datatypes/uuid.md): A 16-byte UUID.
* [`Vec2D`](datatypes/vec2d.md): A vector in 2D space.
* [`Vec3D`](datatypes/vec3d.md): A vector in 3D space.
* [`Vec4D`](datatypes/vec4d.md): A vector in 4D space.
* [`VisibleTimeRange`](datatypes/visible_time_range.md): Visible time range bounds for a specific timeline.

